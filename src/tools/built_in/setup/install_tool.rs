use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;

use crate::{
    automations::{Automation, AutomationEffect, Automations, Trigger},
    bus::Sender,
    config::RuntimeLayout,
    message::ToolDefinition,
    secrets::Secrets,
    tools::{
        self, InternalTool, ToolContext, ToolResponder,
        setup::{
            SetupServices, delete_current_message,
            install_common::{
                SetupConfig, SetupSecret, apply_config_values, config_prompt, config_value, is_yes,
                secret_value, stored_secret_name,
            },
            publish_tool_result, send,
        },
        wasm::{WasmTool, WasmToolConfig, WasmToolManifest},
    },
};

const HANDLE_CONVERSATION_AUTOMATION: &str = "handle-conversation";

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct InstallToolParams {
    /// Tool name to install, for example "web_search".
    pub name: String,
    /// Latest user message. Omitted when setup starts; filled by the temporary
    /// tool_auth automation for follow-up replies.
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ListToolsParams {}

#[derive(Clone)]
struct InstallSession {
    package: ToolPackage,
    stage: InstallStage,
    config_values: BTreeMap<String, serde_json::Value>,
    continuation: Option<ToolResponder>,
}

#[derive(Clone)]
enum InstallStage {
    Confirm,
    Secret { index: usize },
    Config { index: usize },
}

#[derive(Clone)]
struct ToolPackage {
    name: String,
    display_name: String,
    wasm_path: PathBuf,
    config_template_path: PathBuf,
    manifest: ToolInstallManifest,
}

#[derive(Clone, serde::Deserialize)]
struct ToolInstallManifest {
    name: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    secrets: Vec<SetupSecret>,
    #[serde(default)]
    config: Vec<SetupConfig>,
    #[serde(default)]
    setup_url: Option<String>,
}

pub struct InstallToolTool {
    setup: Arc<SetupServices>,
    session: Mutex<Option<InstallSession>>,
}

pub struct ListToolsTool {
    layout: RuntimeLayout,
    secrets: Secrets,
}

pub fn install_packaged_tool(
    layout: &RuntimeLayout,
    name: &str,
) -> Result<WasmToolManifest, String> {
    let name = normalize_tool_name(name)?;
    let tools_dir = layout.tools_dir();
    let wasm_path = tools_dir.join(format!("{name}.wasm"));
    let config_path = tools_dir.join(format!("{name}.config.json"));
    if wasm_path.exists() && config_path.exists() {
        let config_json = std::fs::read_to_string(&config_path).map_err(|err| err.to_string())?;
        let config: WasmToolConfig =
            serde_json::from_str(&config_json).map_err(|err| err.to_string())?;
        return Ok(WasmToolManifest {
            name,
            wasm_path,
            config_path,
            config_json,
            config,
        });
    }

    let package = locate_tool_package(layout, &name)?;
    if !required_config(&package).is_empty() {
        return Err(format!(
            "tool '{}' requires interactive config; use install_tool",
            package.name
        ));
    }
    install_package(layout, &package, &BTreeMap::new())
}

impl ListToolsTool {
    pub fn new(layout: RuntimeLayout, secrets: Secrets) -> Self {
        Self { layout, secrets }
    }

    async fn execute(&self) -> Result<serde_json::Value, String> {
        let installed = installed_tool_names(&self.layout)?
            .into_iter()
            .collect::<BTreeSet<_>>();
        let mut package_names = BTreeSet::new();
        let mut tool_rows = Vec::new();

        for package in discover_tool_packages(&self.layout) {
            let package = package?;
            let installed = installed.contains(&package.name);
            package_names.insert(package.name.clone());
            tool_rows.push(serde_json::json!({
                "name": package.name,
                "display_name": package.display_name,
                "description": package.manifest.description,
                "installed": installed,
                "installable": !installed,
                "required_secrets": secret_statuses(&package, &self.secrets),
                "required_config": config_statuses(&package),
            }));
        }

        for name in installed.difference(&package_names) {
            tool_rows.push(serde_json::json!({
                "name": name,
                "installed": true,
                "installable": false,
                "required_secrets": [],
                "required_config": [],
            }));
        }

        tool_rows.sort_by(|left, right| {
            left["name"]
                .as_str()
                .unwrap_or_default()
                .cmp(right["name"].as_str().unwrap_or_default())
        });

        Ok(serde_json::json!({
            "ok": true,
            "tools": tool_rows,
        }))
    }
}

impl InstallToolTool {
    pub fn new(setup: Arc<SetupServices>) -> Self {
        Self {
            setup,
            session: Mutex::new(None),
        }
    }

    fn automation(tool_name: &str) -> Automation {
        Automation {
            name: format!("tool_auth:{tool_name}"),
            trigger: Trigger::MessageIn {
                sender: Some(Sender::User),
                channel: None,
            },
            effect: AutomationEffect::Tool {
                tool: "install_tool".to_string(),
                args: serde_json::json!({
                    "name": tool_name,
                    "message": "{{event.text}}",
                }),
            },
        }
    }

    async fn execute(
        &self,
        params: InstallToolParams,
        ctx: &ToolContext,
        responder: ToolResponder,
    ) -> Result<serde_json::Value, String> {
        let name = normalize_tool_name(&params.name)?;
        match params.message {
            None => self.start(&name, ctx, responder).await,
            Some(message) => self.continue_setup(&name, &message, ctx).await,
        }
    }

    async fn start(
        &self,
        name: &str,
        ctx: &ToolContext,
        responder: ToolResponder,
    ) -> Result<serde_json::Value, String> {
        let package = locate_tool_package(&self.setup.layout, name)?;
        *self.session.lock().expect("install tool session lock") = Some(InstallSession {
            package,
            stage: InstallStage::Confirm,
            config_values: BTreeMap::new(),
            continuation: Some(responder),
        });
        self.setup
            .automations
            .delete(HANDLE_CONVERSATION_AUTOMATION)?;
        self.setup.automations.add(Self::automation(name))?;
        send(
            &self.setup.tools,
            &format!("Install the {name} tool? Reply yes to continue, or no to cancel."),
            ctx,
        )
        .await?;
        Ok(serde_json::json!({
            "ok": true,
            "tool": name,
            "message": "deterministic tool setup prompt sent"
        }))
    }

    async fn continue_setup(
        &self,
        name: &str,
        message: &str,
        ctx: &ToolContext,
    ) -> Result<serde_json::Value, String> {
        let session = self
            .session
            .lock()
            .expect("install tool session lock")
            .clone()
            .ok_or_else(|| "tool setup is not active; ask to install the tool again".to_string())?;
        if session.package.name != name {
            return Err(format!(
                "tool setup is active for '{}', not '{name}'",
                session.package.name
            ));
        }

        match session.stage {
            InstallStage::Confirm => {
                if !is_yes(message) {
                    self.cancel(session, ctx).await?;
                    return Ok(serde_json::json!({ "ok": true, "cancelled": true }));
                }
                let mut session = session;
                let secrets = required_secrets(&session.package);
                if secrets.is_empty() {
                    self.prompt_config_or_finish(session, ctx).await?;
                    return Ok(serde_json::json!({ "ok": true }));
                }
                session.stage = InstallStage::Secret { index: 0 };
                self.save_session(session.clone());
                send(
                    &self.setup.tools,
                    &secret_prompt(&session.package, &secrets[0]),
                    ctx,
                )
                .await?;
            }
            InstallStage::Secret { index } => {
                let mut session = session;
                let secrets = required_secrets(&session.package);
                let secret = &secrets[index];
                let value = secret_value(secret, message)?;
                self.setup
                    .secrets
                    .set(&stored_secret_name(&secret.name), &value)?;
                delete_current_message(&self.setup.tools, ctx, "install_tool").await;

                let next = index + 1;
                if let Some(secret) = secrets.get(next) {
                    session.stage = InstallStage::Secret { index: next };
                    self.save_session(session.clone());
                    send(
                        &self.setup.tools,
                        &secret_prompt(&session.package, secret),
                        ctx,
                    )
                    .await?;
                } else {
                    self.prompt_config_or_finish(session, ctx).await?;
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
                    send(&self.setup.tools, config_prompt(config), ctx).await?;
                } else {
                    self.complete(session, ctx).await?;
                }
            }
        }

        Ok(serde_json::json!({ "ok": true }))
    }

    fn save_session(&self, session: InstallSession) {
        *self.session.lock().expect("install tool session lock") = Some(session);
    }

    async fn prompt_config_or_finish(
        &self,
        mut session: InstallSession,
        ctx: &ToolContext,
    ) -> Result<(), String> {
        let configs = required_config(&session.package);
        let Some(config) = configs.first() else {
            return self.complete(session, ctx).await;
        };
        session.stage = InstallStage::Config { index: 0 };
        self.save_session(session);
        send(&self.setup.tools, config_prompt(config), ctx).await
    }

    async fn complete(&self, session: InstallSession, ctx: &ToolContext) -> Result<(), String> {
        let manifest =
            install_package(&self.setup.layout, &session.package, &session.config_values)?;
        let tool = WasmTool::from_manifest(manifest, self.setup.secrets.tools.clone())?;
        self.setup.tools.registry.add(Arc::new(tool))?;
        self.finish(session, ctx).await
    }

    async fn cancel(&self, session: InstallSession, ctx: &ToolContext) -> Result<(), String> {
        let name = session.package.name.clone();
        let continuation = session.continuation.clone();
        *self.session.lock().expect("install tool session lock") = None;
        self.setup
            .automations
            .delete(&format!("tool_auth:{name}"))?;
        if !self
            .setup
            .automations
            .contains(HANDLE_CONVERSATION_AUTOMATION)
        {
            self.setup
                .automations
                .add(Automations::handle_conversation())?;
        }
        send(
            &self.setup.tools,
            "Tool installation cancelled. I will answer normally again.",
            ctx,
        )
        .await?;
        publish_tool_result(
            continuation.as_ref(),
            serde_json::json!({ "ok": true, "tool": name, "cancelled": true }),
        );
        Ok(())
    }

    async fn finish(&self, session: InstallSession, ctx: &ToolContext) -> Result<(), String> {
        let name = session.package.name.clone();
        let continuation = session.continuation.clone();
        *self.session.lock().expect("install tool session lock") = None;
        self.setup
            .automations
            .delete(&format!("tool_auth:{name}"))?;
        if !self
            .setup
            .automations
            .contains(HANDLE_CONVERSATION_AUTOMATION)
        {
            self.setup
                .automations
                .add(Automations::handle_conversation())?;
        }
        send(
            &self.setup.tools,
            &format!("The {name} tool is installed."),
            ctx,
        )
        .await?;
        publish_tool_result(
            continuation.as_ref(),
            serde_json::json!({ "ok": true, "tool": name, "installed": true }),
        );
        Ok(())
    }
}

#[async_trait]
impl InternalTool for ListToolsTool {
    fn name(&self) -> &str {
        "list_tools"
    }

    fn tool_definition(&self) -> ToolDefinition {
        tools::tool_definition::<ListToolsParams>(
            self.name(),
            "List Warden external WASM tools, including installed runtime tools \
             and installable local tool packages. Use this before install_tool \
             when the user asks what tools are available.",
        )
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ToolContext,
        responder: ToolResponder,
    ) -> Result<(), String> {
        let output = ListToolsTool::execute(self).await?;
        responder.respond(output)?;
        Ok(())
    }
}

#[async_trait]
impl InternalTool for InstallToolTool {
    fn name(&self) -> &str {
        "install_tool"
    }

    fn tool_definition(&self) -> ToolDefinition {
        tools::tool_definition::<InstallToolParams>(
            self.name(),
            "Install a local external WASM tool package. When called with only \
             `name`, this starts a deterministic setup flow that asks the user \
             for confirmation and required secrets without exposing those \
             secret replies to the LLM.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
        responder: ToolResponder,
    ) -> Result<(), String> {
        let params: InstallToolParams =
            serde_json::from_value(args).map_err(|err| err.to_string())?;
        let starts_setup = params.message.is_none();
        let result = InstallToolTool::execute(self, params, ctx, responder.clone()).await?;
        if starts_setup && result.get("ok").and_then(|ok| ok.as_bool()) == Some(true) {
            return Ok(());
        }
        responder.respond(result)?;
        Ok(())
    }
}

fn normalize_tool_name(name: &str) -> Result<String, String> {
    let name = name.trim().to_ascii_lowercase().replace('-', "_");
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(
            "tool name must contain only lowercase letters, digits, '_' or '-'".to_string(),
        );
    }
    Ok(name)
}

fn locate_tool_package(layout: &RuntimeLayout, name: &str) -> Result<ToolPackage, String> {
    let package_dir_name = name.replace('_', "-");
    let source_dir = tool_source_dirs(layout)
        .into_iter()
        .flat_map(|dir| [dir.join(name), dir.join(&package_dir_name)])
        .find(|dir| dir.exists())
        .ok_or_else(|| format!("unknown installable tool: {name}"))?;
    let actual_dir_name = source_dir
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("invalid tool package path: {}", source_dir.display()))?
        .to_string();
    let manifest_path = source_dir.join(format!("{actual_dir_name}.install.json"));
    if !manifest_path.exists() {
        return Err(format!(
            "tool '{name}' is missing {}",
            manifest_path.display()
        ));
    }
    let manifest: ToolInstallManifest = serde_json::from_str(
        &std::fs::read_to_string(&manifest_path).map_err(|err| err.to_string())?,
    )
    .map_err(|err| err.to_string())?;
    if normalize_tool_name(&manifest.name)? != name {
        return Err(format!(
            "install manifest declares tool '{}' but package is '{name}'",
            manifest.name
        ));
    }
    let wasm_path = locate_wasm(&source_dir, &actual_dir_name)?;
    let config_template_path = locate_config_template(&source_dir, &actual_dir_name)?;
    let display_name = manifest
        .display_name
        .clone()
        .unwrap_or_else(|| format!("{name} tool"));
    Ok(ToolPackage {
        name: name.to_string(),
        display_name,
        wasm_path,
        config_template_path,
        manifest,
    })
}

fn discover_tool_packages(layout: &RuntimeLayout) -> Vec<Result<ToolPackage, String>> {
    let mut packages = Vec::new();
    for source_root in tool_source_dirs(layout) {
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
            let Some(dir_name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !entry
                .path()
                .join(format!("{dir_name}.install.json"))
                .exists()
            {
                continue;
            }
            let name = dir_name.replace('-', "_");
            packages.push(locate_tool_package(layout, &name));
        }
    }
    packages
}

fn installed_tool_names(layout: &RuntimeLayout) -> Result<Vec<String>, String> {
    let mut names = crate::tools::wasm::discover(layout)
        .map_err(|err| err.to_string())?
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    Ok(names)
}

fn tool_source_dirs(layout: &RuntimeLayout) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(path) = std::env::var("WARDEN_TOOLS_SRC") {
        dirs.push(PathBuf::from(path));
    }
    dirs.push(layout.root().join("tool-store"));
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dirs.push(manifest_dir.join("tools-src"));
    dirs
}

fn locate_wasm(source_dir: &Path, dir_name: &str) -> Result<PathBuf, String> {
    let flat = source_dir.join(format!("{dir_name}.wasm"));
    if flat.exists() {
        return Ok(flat);
    }
    let crate_name = dir_name.replace('-', "_");
    let built = source_dir
        .join("target")
        .join("wasm32-wasip2")
        .join("release")
        .join(format!("{crate_name}.wasm"));
    if built.exists() {
        return Ok(built);
    }
    Err(format!(
        "tool '{dir_name}' has no built WASM artifact in {}",
        source_dir.display()
    ))
}

fn locate_config_template(source_dir: &Path, dir_name: &str) -> Result<PathBuf, String> {
    let config = source_dir.join(format!("{dir_name}.config.json"));
    if config.exists() {
        return Ok(config);
    }
    let example = source_dir.join(format!("{dir_name}.config.example.json"));
    if example.exists() {
        return Ok(example);
    }
    Err(format!(
        "tool '{dir_name}' has no config template in {}",
        source_dir.display()
    ))
}

fn required_secrets(package: &ToolPackage) -> Vec<SetupSecret> {
    package.manifest.secrets.clone()
}

fn required_config(package: &ToolPackage) -> Vec<SetupConfig> {
    package.manifest.config.clone()
}

fn secret_statuses(package: &ToolPackage, secrets_store: &Secrets) -> Vec<serde_json::Value> {
    required_secrets(package)
        .into_iter()
        .map(|secret| {
            let secret_name = stored_secret_name(&secret.name);
            serde_json::json!({
                "name": secret.name,
                "secret": secret_name,
                "configured": secrets_store.exists(&secret_name),
                "optional": secret.optional,
            })
        })
        .collect()
}

fn config_statuses(package: &ToolPackage) -> Vec<serde_json::Value> {
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

fn secret_prompt(package: &ToolPackage, secret: &SetupSecret) -> String {
    let mut prompt = format!(
        "{}: {}",
        package.display_name,
        secret.prompt.as_deref().unwrap_or(&secret.name)
    );
    if let Some(setup_url) = &package.manifest.setup_url {
        prompt.push_str(&format!(" Setup: {setup_url}"));
    }
    prompt
}

fn install_package(
    layout: &RuntimeLayout,
    package: &ToolPackage,
    config_values: &BTreeMap<String, serde_json::Value>,
) -> Result<WasmToolManifest, String> {
    let tools_dir = layout.tools_dir();
    std::fs::create_dir_all(&tools_dir).map_err(|err| err.to_string())?;
    let wasm_dst = tools_dir.join(format!("{}.wasm", package.name));
    let config_dst = tools_dir.join(format!("{}.config.json", package.name));
    if wasm_dst.exists() || config_dst.exists() {
        return Err(format!("tool '{}' is already installed", package.name));
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
    let config_json = serde_json::to_string_pretty(&config).map_err(|err| err.to_string())?;
    std::fs::write(&config_dst, &config_json).map_err(|err| err.to_string())?;
    let parsed: WasmToolConfig =
        serde_json::from_str(&config_json).map_err(|err| err.to_string())?;
    Ok(WasmToolManifest {
        name: package.name.clone(),
        wasm_path: wasm_dst,
        config_path: config_dst,
        config_json,
        config: parsed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_hyphenated_tool_name() {
        assert_eq!(normalize_tool_name("web-search").unwrap(), "web_search");
    }
}
