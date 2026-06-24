use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;

use crate::{
    automations::{Automation, AutomationEffect, Automations, Trigger},
    bus::Sender,
    channels,
    config::RuntimeLayout,
    message::ToolDefinition,
    tools::{
        self, InternalTool, InternalTools, ToolContext, ToolOutput,
        setup::{
            SetupServices, delete_current_message,
            install_common::{
                SetupConfig, SetupSecret, apply_config_values, config_prompt, config_value,
                env_secret_name, is_yes, reload_env, secret_value, write_env_secret,
            },
            send,
        },
    },
};

const HANDLE_CONVERSATION_AUTOMATION: &str = "handle-conversation";
const NGROK_SECRET: &str = "WARDEN_NGROK_TOKEN";
const BUILTIN_CHANNELS: &[&str] = &["tui"];

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct InstallChannelParams {
    /// Channel name to install, for example "telegram".
    pub name: String,
    /// Latest user message. Omitted when the LLM starts setup; filled by the
    /// temporary channel_auth automation for follow-up replies.
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ListChannelsParams {}

#[derive(Clone)]
struct InstallSession {
    package: ChannelPackage,
    stage: InstallStage,
    config_values: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone)]
enum InstallStage {
    Confirm,
    Secret { index: usize },
    Config { index: usize },
}

#[derive(Clone)]
struct ChannelPackage {
    name: String,
    display_name: String,
    wasm_path: PathBuf,
    config_template_path: PathBuf,
    manifest: ChannelInstallManifest,
}

#[derive(Clone, serde::Deserialize)]
struct ChannelInstallManifest {
    name: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    external: bool,
    #[serde(default)]
    secrets: Vec<SetupSecret>,
    #[serde(default)]
    config: Vec<SetupConfig>,
    #[serde(default)]
    setup_url: Option<String>,
}

pub struct InstallChannelTool {
    setup: Arc<SetupServices>,
    session: Mutex<Option<InstallSession>>,
}

pub struct ListChannelsTool {
    layout: RuntimeLayout,
}

impl ListChannelsTool {
    pub fn new(layout: RuntimeLayout) -> Self {
        Self { layout }
    }

    async fn execute(&self) -> Result<serde_json::Value, String> {
        let installed = installed_channel_names(&self.layout)?
            .into_iter()
            .collect::<BTreeSet<_>>();
        let mut package_names = BTreeSet::new();
        let mut channel_rows = Vec::new();

        for package in discover_channel_packages(&self.layout) {
            let package = package?;
            let installed = installed.contains(&package.name);
            package_names.insert(package.name.clone());
            channel_rows.push(serde_json::json!({
                "name": package.name,
                "display_name": package.display_name,
                "installed": installed,
                "installable": !installed,
                "external": package.manifest.external,
                "required_secrets": secret_statuses(&package),
                "required_config": config_statuses(&package),
            }));
        }

        for name in installed.difference(&package_names) {
            channel_rows.push(serde_json::json!({
                "name": name,
                "installed": true,
                "installable": false,
                "external": null,
                "required_secrets": [],
                "required_config": [],
            }));
        }

        channel_rows.sort_by(|left, right| {
            left["name"]
                .as_str()
                .unwrap_or_default()
                .cmp(right["name"].as_str().unwrap_or_default())
        });

        Ok(serde_json::json!({
            "ok": true,
            "channels": channel_rows,
        }))
    }
}

impl InstallChannelTool {
    pub fn new(setup: Arc<SetupServices>) -> Self {
        Self {
            setup,
            session: Mutex::new(None),
        }
    }

    fn automation(channel_name: &str) -> Automation {
        Automation {
            name: format!("channel_auth:{channel_name}"),
            trigger: Trigger::MessageIn {
                sender: Some(Sender::User),
                channel: None,
            },
            effect: AutomationEffect::Tool {
                tool: "install_channel".to_string(),
                args: serde_json::json!({
                    "name": channel_name,
                    "message": "{{event.text}}",
                }),
            },
        }
    }

    async fn execute(
        &self,
        params: InstallChannelParams,
        ctx: &ToolContext,
    ) -> Result<serde_json::Value, String> {
        let name = normalize_channel_name(&params.name)?;
        match params.message {
            None => self.start(&name, ctx).await,
            Some(message) => self.continue_setup(&name, &message, ctx).await,
        }
    }

    async fn start(&self, name: &str, ctx: &ToolContext) -> Result<serde_json::Value, String> {
        let tools = Arc::as_ref(&self.setup.tools);
        let automations = &self.setup.automations;
        let layout = &self.setup.layout;
        let package = locate_channel_package(layout, name)?;
        *self.session.lock().expect("install channel session lock") = Some(InstallSession {
            package,
            stage: InstallStage::Confirm,
            config_values: BTreeMap::new(),
        });
        automations.delete(HANDLE_CONVERSATION_AUTOMATION)?;
        automations.add(Self::automation(name))?;
        send(
            tools,
            &format!("Install the {name} channel? Reply yes to continue, or no to cancel."),
            ctx,
        )
        .await?;
        Ok(serde_json::json!({
            "ok": true,
            "channel": name,
            "message": "deterministic channel setup prompt sent"
        }))
    }

    async fn continue_setup(
        &self,
        name: &str,
        message: &str,
        ctx: &ToolContext,
    ) -> Result<serde_json::Value, String> {
        let tools = Arc::as_ref(&self.setup.tools);
        let automations = &self.setup.automations;
        let layout = &self.setup.layout;
        let env_path = &self.setup.env_path;
        let session = self
            .session
            .lock()
            .expect("install channel session lock")
            .clone()
            .ok_or_else(|| {
                "channel setup is not active; ask to install the channel again".to_string()
            })?;
        if session.package.name != name {
            return Err(format!(
                "channel setup is active for '{}', not '{name}'",
                session.package.name
            ));
        }

        match session.stage {
            InstallStage::Confirm => {
                if !is_yes(message) {
                    self.cancel(&session.package.name, tools, automations, ctx)
                        .await?;
                    return Ok(serde_json::json!({ "ok": true, "cancelled": true }));
                }
                let mut session = session;
                let secrets = required_secrets(&session.package);
                if secrets.is_empty() {
                    self.prompt_config_or_finish(session, layout, tools, automations, ctx)
                        .await?;
                    return Ok(serde_json::json!({ "ok": true }));
                }
                session.stage = InstallStage::Secret { index: 0 };
                self.save_session(session.clone());
                send(tools, &secret_prompt(&session.package, &secrets[0]), ctx).await?;
            }
            InstallStage::Secret { index } => {
                let mut session = session;
                let secrets = required_secrets(&session.package);
                let secret = &secrets[index];
                let value = secret_value(secret, message)?;
                write_env_secret(env_path, &env_secret_name(&secret.name), &value)?;
                reload_env(env_path)?;
                delete_current_message(tools, ctx, "install_channel").await;

                let next = index + 1;
                if let Some(secret) = secrets.get(next) {
                    session.stage = InstallStage::Secret { index: next };
                    self.save_session(session.clone());
                    send(tools, &secret_prompt(&session.package, secret), ctx).await?;
                } else {
                    self.prompt_config_or_finish(session, layout, tools, automations, ctx)
                        .await?;
                }
            }
            InstallStage::Config { index } => {
                let mut session = session;
                let configs = required_config(&session.package);
                let config = &configs[index];
                session
                    .config_values
                    .insert(config.path.clone(), config_value(config, message)?);

                let next = index + 1;
                if let Some(config) = configs.get(next) {
                    session.stage = InstallStage::Config { index: next };
                    self.save_session(session.clone());
                    send(tools, config_prompt(config), ctx).await?;
                } else {
                    self.complete(session, layout, tools, automations, ctx)
                        .await?;
                }
            }
        }

        Ok(serde_json::json!({ "ok": true }))
    }

    fn save_session(&self, session: InstallSession) {
        *self.session.lock().expect("install channel session lock") = Some(session);
    }

    async fn prompt_config_or_finish(
        &self,
        mut session: InstallSession,
        layout: &RuntimeLayout,
        tools: &InternalTools,
        automations: &Arc<Automations>,
        ctx: &ToolContext,
    ) -> Result<(), String> {
        let configs = required_config(&session.package);
        let Some(config) = configs.first() else {
            return self
                .complete(session, layout, tools, automations, ctx)
                .await;
        };
        session.stage = InstallStage::Config { index: 0 };
        self.save_session(session);
        send(tools, config_prompt(config), ctx).await
    }

    async fn complete(
        &self,
        session: InstallSession,
        layout: &RuntimeLayout,
        tools: &InternalTools,
        automations: &Arc<Automations>,
        ctx: &ToolContext,
    ) -> Result<(), String> {
        install_package(layout, &session.package, &session.config_values)?;
        self.activate_package(&session.package).await?;
        self.finish(&session.package.name, tools, automations, ctx)
            .await
    }

    async fn cancel(
        &self,
        name: &str,
        tools: &InternalTools,
        automations: &Arc<Automations>,
        ctx: &ToolContext,
    ) -> Result<(), String> {
        *self.session.lock().expect("install channel session lock") = None;
        automations.delete(&format!("channel_auth:{name}"))?;
        if !automations.contains(HANDLE_CONVERSATION_AUTOMATION) {
            automations.add(Automations::handle_conversation())?;
        }
        send(
            tools,
            "Channel installation cancelled. I will answer normally again.",
            ctx,
        )
        .await
    }

    async fn finish(
        &self,
        name: &str,
        tools: &InternalTools,
        automations: &Arc<Automations>,
        ctx: &ToolContext,
    ) -> Result<(), String> {
        *self.session.lock().expect("install channel session lock") = None;
        automations.delete(&format!("channel_auth:{name}"))?;
        if !automations.contains(HANDLE_CONVERSATION_AUTOMATION) {
            automations.add(Automations::handle_conversation())?;
        }
        send(
            tools,
            &format!("The {name} channel is installed and running."),
            ctx,
        )
        .await
    }

    async fn activate_package(&self, package: &ChannelPackage) -> Result<(), String> {
        let runtime = self.setup.channel_runtime()?;
        runtime
            .activate_installed_channel(&package.name, package.manifest.external)
            .await
    }
}

#[async_trait]
impl InternalTool for ListChannelsTool {
    fn name(&self) -> &str {
        "list_channels"
    }

    fn tool_definition(&self) -> ToolDefinition {
        tools::tool_definition::<ListChannelsParams>(
            self.name(),
            "List Warden channels, including installed runtime channels and \
             installable local channel packages. Use this before install_channel \
             when the user asks what channels are available.",
        )
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput, String> {
        ListChannelsTool::execute(self).await
    }
}

#[async_trait]
impl InternalTool for InstallChannelTool {
    fn name(&self) -> &str {
        "install_channel"
    }

    fn tool_definition(&self) -> ToolDefinition {
        tools::tool_definition::<InstallChannelParams>(
            self.name(),
            "Install a local channel package. When called with only `name`, \
             this starts a deterministic setup flow that asks the user for \
             confirmation and required secrets without exposing those secret \
             replies to the LLM.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, String> {
        tools::execute_typed(args, |params| async {
            InstallChannelTool::execute(self, params, ctx).await
        })
        .await
    }
}

fn normalize_channel_name(name: &str) -> Result<String, String> {
    let name = name.trim().to_ascii_lowercase();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        return Err(
            "channel name must contain only lowercase letters, digits, '_' or '-'".to_string(),
        );
    }
    Ok(name)
}

fn locate_channel_package(layout: &RuntimeLayout, name: &str) -> Result<ChannelPackage, String> {
    let source_dir = channel_source_dirs(layout)
        .into_iter()
        .map(|dir| dir.join(name))
        .find(|dir| dir.exists())
        .ok_or_else(|| format!("unknown installable channel: {name}"))?;
    let manifest_path = source_dir.join(format!("{name}.install.json"));
    if !manifest_path.exists() {
        return Err(format!(
            "channel '{name}' is missing {}",
            manifest_path.display()
        ));
    }
    let manifest = parse_install_manifest(
        &std::fs::read_to_string(&manifest_path).map_err(|err| err.to_string())?,
    )?;
    if manifest.name != name {
        return Err(format!(
            "install manifest declares channel '{}' but package is '{name}'",
            manifest.name
        ));
    }
    let wasm_path = locate_wasm(&source_dir, name)?;
    let config_template_path = locate_config_template(&source_dir, name)?;
    let display_name = manifest
        .display_name
        .clone()
        .unwrap_or_else(|| format!("{name} channel"));
    Ok(ChannelPackage {
        name: name.to_string(),
        display_name,
        wasm_path,
        config_template_path,
        manifest,
    })
}

fn discover_channel_packages(layout: &RuntimeLayout) -> Vec<Result<ChannelPackage, String>> {
    let mut packages = Vec::new();
    for source_root in channel_source_dirs(layout) {
        let Ok(entries) = std::fs::read_dir(source_root) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !entry.path().join(format!("{name}.install.json")).exists() {
                continue;
            }
            packages.push(locate_channel_package(layout, &name));
        }
    }
    packages
}

fn installed_channel_names(layout: &RuntimeLayout) -> Result<Vec<String>, String> {
    let mut names = BUILTIN_CHANNELS
        .iter()
        .map(|name| name.to_string())
        .collect::<Vec<_>>();
    names.extend(
        channels::config::discover(layout)
            .map_err(|err| err.to_string())?
            .into_iter()
            .map(|channel| channel.name),
    );
    names.sort();
    names.dedup();
    Ok(names)
}

fn channel_source_dirs(layout: &RuntimeLayout) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(path) = std::env::var("WARDEN_CHANNELS_SRC") {
        dirs.push(PathBuf::from(path));
    }
    dirs.push(layout.root().join("channel-store"));
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dirs.push(manifest_dir.join("channels-src"));
    dirs
}

fn locate_wasm(source_dir: &Path, name: &str) -> Result<PathBuf, String> {
    let flat = source_dir.join(format!("{name}.wasm"));
    if flat.exists() {
        return Ok(flat);
    }
    let built = source_dir
        .join("target")
        .join("wasm32-wasip2")
        .join("release")
        .join(format!("{}_channel.wasm", name.replace('-', "_")));
    if built.exists() {
        return Ok(built);
    }
    Err(format!(
        "channel '{name}' has no built WASM artifact in {}",
        source_dir.display()
    ))
}

fn locate_config_template(source_dir: &Path, name: &str) -> Result<PathBuf, String> {
    let config = source_dir.join(format!("{name}.config.json"));
    if config.exists() {
        return Ok(config);
    }
    let example = source_dir.join(format!("{name}.config.example.json"));
    if example.exists() {
        return Ok(example);
    }
    Err(format!(
        "channel '{name}' has no config template in {}",
        source_dir.display()
    ))
}

fn parse_install_manifest(json: &str) -> Result<ChannelInstallManifest, String> {
    serde_json::from_str(json).map_err(|err| err.to_string())
}

fn required_secrets(package: &ChannelPackage) -> Vec<SetupSecret> {
    let mut secrets = Vec::new();
    if package.manifest.external
        && std::env::var(NGROK_SECRET)
            .ok()
            .is_none_or(|value| value.trim().is_empty())
    {
        secrets.push(SetupSecret {
            name: NGROK_SECRET.to_string(),
            prompt: Some(
                "Paste your ngrok auth token so Warden can expose webhook channels.".to_string(),
            ),
            optional: false,
            auto_generate: None,
        });
    }
    secrets.extend(package.manifest.secrets.clone());
    secrets
}

fn required_config(package: &ChannelPackage) -> Vec<SetupConfig> {
    package.manifest.config.clone()
}

fn secret_statuses(package: &ChannelPackage) -> Vec<serde_json::Value> {
    required_secrets(package)
        .into_iter()
        .map(|secret| {
            let env_name = env_secret_name(&secret.name);
            serde_json::json!({
                "name": secret.name,
                "env": env_name,
                "configured": std::env::var(&env_name)
                    .ok()
                    .is_some_and(|value| !value.trim().is_empty()),
                "optional": secret.optional,
                "auto_generate": secret.auto_generate.is_some(),
            })
        })
        .collect()
}

fn config_statuses(package: &ChannelPackage) -> Vec<serde_json::Value> {
    required_config(package)
        .into_iter()
        .map(|config| {
            serde_json::json!({
                "path": config.path,
                "type": config.value_type.as_str(),
                "prompt": config.prompt.as_deref().unwrap_or(&config.path),
            })
        })
        .collect()
}

fn secret_prompt(package: &ChannelPackage, secret: &SetupSecret) -> String {
    let mut prompt = format!(
        "{}: {}",
        package.display_name,
        secret.prompt.as_deref().unwrap_or(&secret.name)
    );
    if secret.name == NGROK_SECRET {
        prompt.push_str(" You can find it in your ngrok dashboard.");
    } else if let Some(setup_url) = &package.manifest.setup_url {
        prompt.push_str(&format!(" Setup: {setup_url}"));
    }
    prompt
}

fn install_package(
    layout: &RuntimeLayout,
    package: &ChannelPackage,
    config_values: &BTreeMap<String, serde_json::Value>,
) -> Result<(), String> {
    let channels_dir = layout.channels_dir();
    std::fs::create_dir_all(&channels_dir).map_err(|err| err.to_string())?;
    let wasm_dst = channels_dir.join(format!("{}.wasm", package.name));
    let config_dst = channels_dir.join(format!("{}.config.json", package.name));
    if wasm_dst.exists() || config_dst.exists() {
        return Err(format!("channel '{}' is already installed", package.name));
    }
    let mut config: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&package.config_template_path).map_err(|err| {
            format!(
                "could not read {}: {err}",
                package.config_template_path.display()
            )
        })?,
    )
    .map_err(|err| {
        format!(
            "could not parse {}: {err}",
            package.config_template_path.display()
        )
    })?;
    apply_config_values(&mut config, config_values)?;

    std::fs::copy(&package.wasm_path, &wasm_dst).map_err(|err| err.to_string())?;
    std::fs::write(
        &config_dst,
        serde_json::to_string_pretty(&config).map_err(|err| err.to_string())?,
    )
    .map_err(|err| err.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_warden_install_manifest() {
        let manifest = parse_install_manifest(
            r#"{
              "name": "telegram",
              "display_name": "Telegram",
              "external": true,
              "setup_url": "https://t.me/BotFather",
              "secrets": [
                { "name": "telegram_bot_token", "prompt": "Bot token" },
                { "name": "telegram_webhook_secret", "prompt": "Webhook", "optional": true, "auto_generate": { "length": 4 } }
              ],
              "config": [
                { "path": "/owner_id", "prompt": "Owner id", "type": "string" }
              ]
            }"#,
        )
        .unwrap();

        assert_eq!(manifest.name, "telegram");
        assert!(manifest.external);
        assert_eq!(manifest.secrets.len(), 2);
        assert_eq!(manifest.config.len(), 1);
        assert_eq!(manifest.config[0].path, "/owner_id");
    }
}
