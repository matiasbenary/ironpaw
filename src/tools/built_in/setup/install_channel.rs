use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;

use crate::{
    automations::{Automation, AutomationEffect, Automations, Trigger},
    channels,
    config::RuntimeLayout,
    message::ToolDefinition,
    secrets::Secrets,
    tools::{
        self, InternalTool, InternalTools, ToolContext, ToolResponder,
        setup::{
            SetupServices, delete_current_message,
            install_common::{
                SetupConfig, SetupSecret, apply_config_values, config_prompt, config_value,
                generate_secret, is_yes, secret_value, stored_secret_name,
            },
            publish_tool_result, send,
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
    /// Channel that produced this setup reply.
    #[serde(default)]
    pub channel: Option<String>,
    /// Stable user id emitted by the channel that produced this setup reply.
    #[serde(default)]
    pub channel_user_id: Option<String>,
    /// Latest user message. Omitted when the LLM starts setup; filled by the
    /// temporary channel_auth automation for follow-up replies.
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct InstallChannelStartParams {
    /// Channel name to install, for example "telegram".
    pub name: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ListChannelsParams {}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct RemoveChannelParams {
    /// Channel name to remove, for example "telegram".
    pub name: String,
}

#[derive(Clone)]
struct InstallSession {
    package: ChannelPackage,
    secrets: Vec<SetupSecret>,
    stage: InstallStage,
    config_values: BTreeMap<String, serde_json::Value>,
    setup_conversation: Option<SetupConversation>,
    continuation: Option<ToolResponder>,
}

#[derive(Clone)]
struct SetupConversation {
    channel: String,
    chat_id: String,
    thread_id: Option<String>,
}

#[derive(Clone)]
enum InstallStage {
    Confirm,
    Secret { index: usize },
    Config { index: usize },
    Verify { activated: bool, code: String },
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

pub struct RemoveChannelTool {
    setup: Arc<SetupServices>,
}

pub struct ListChannelsTool {
    layout: RuntimeLayout,
    secrets: Secrets,
}

impl ListChannelsTool {
    pub fn new(layout: RuntimeLayout, secrets: Secrets) -> Self {
        Self { layout, secrets }
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
                "required_secrets": secret_statuses(&package, &self.secrets),
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
                sender: None,
                channel: None,
            },
            effect: AutomationEffect::Tool {
                tool: "install_channel".to_string(),
                args: serde_json::json!({
                    "name": channel_name,
                    "channel": "{{event.channel}}",
                    "channel_user_id": "{{event.channel_user_id}}",
                    "message": "{{event.text}}",
                }),
            },
        }
    }

    async fn execute(
        &self,
        params: InstallChannelParams,
        ctx: &ToolContext,
        responder: ToolResponder,
    ) -> Result<serde_json::Value, String> {
        let name = normalize_channel_name(&params.name)?;
        let channel = params.channel.as_deref();
        let channel_user_id = params.channel_user_id.as_deref();
        match params.message {
            Some(message) if self.active_session_name().is_some() => {
                self.continue_setup(&name, channel, channel_user_id, &message, ctx)
                    .await
            }
            _ => self.start(&name, ctx, responder).await,
        }
    }

    fn active_session_name(&self) -> Option<String> {
        self.session
            .lock()
            .expect("install channel session lock")
            .as_ref()
            .map(|session| session.package.name.clone())
    }

    async fn start(
        &self,
        name: &str,
        ctx: &ToolContext,
        responder: ToolResponder,
    ) -> Result<serde_json::Value, String> {
        let tools = Arc::as_ref(&self.setup.tools);
        let automations = &self.setup.automations;
        let layout = &self.setup.layout;
        let package = locate_channel_package(layout, name)?;
        if installed_package_exists(layout, &package) {
            send(
                tools,
                &format!(
                    "The {name} channel is already installed. Ask me to remove it first if you want to reinstall it."
                ),
                ctx,
            )
            .await?;
            return Ok(serde_json::json!({
                "ok": true,
                "channel": name,
                "already_installed": true,
            }));
        }
        let secrets = required_secrets(&package, &self.setup.secrets);
        *self.session.lock().expect("install channel session lock") = Some(InstallSession {
            package,
            secrets,
            stage: InstallStage::Confirm,
            config_values: BTreeMap::new(),
            setup_conversation: setup_conversation(ctx),
            continuation: Some(responder),
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
        channel: Option<&str>,
        channel_user_id: Option<&str>,
        message: &str,
        ctx: &ToolContext,
    ) -> Result<serde_json::Value, String> {
        let tools = Arc::as_ref(&self.setup.tools);
        let automations = &self.setup.automations;
        let layout = &self.setup.layout;
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
                    self.cancel(session, tools, automations, ctx).await?;
                    return Ok(serde_json::json!({ "ok": true, "cancelled": true }));
                }
                let mut session = session;
                if session.secrets.is_empty() {
                    self.prompt_config_or_finish(session, layout, tools, ctx)
                        .await?;
                    return Ok(serde_json::json!({ "ok": true }));
                }
                session.stage = InstallStage::Secret { index: 0 };
                self.save_session(session.clone());
                send(
                    tools,
                    &secret_prompt(&session.package, &session.secrets[0]),
                    ctx,
                )
                .await?;
            }
            InstallStage::Secret { index } => {
                let mut session = session;
                let secret = &session.secrets[index];
                let value = secret_value(secret, message)?;
                self.setup
                    .secrets
                    .set(&stored_secret_name(&secret.name), &value)?;
                delete_current_message(tools, ctx, "install_channel").await;

                let next = index + 1;
                if let Some(secret) = session.secrets.get(next) {
                    session.stage = InstallStage::Secret { index: next };
                    self.save_session(session.clone());
                    send(tools, &secret_prompt(&session.package, secret), ctx).await?;
                } else {
                    self.prompt_config_or_finish(session, layout, tools, ctx)
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
                    self.complete(session, layout, tools, ctx).await?;
                }
            }
            InstallStage::Verify {
                activated,
                ref code,
            } => {
                if activated
                    && channel == Some(session.package.name.as_str())
                    && message.trim().eq_ignore_ascii_case(code)
                {
                    let Some(channel_user_id) = channel_user_id else {
                        send(
                            tools,
                            "I received the verification code, but this channel did not provide a stable user id. Continue setup from the original conversation.",
                            ctx,
                        )
                        .await?;
                        return Ok(serde_json::json!({ "ok": false, "verified": false }));
                    };
                    set_channel_owner_id(layout, &session.package, channel_user_id)?;
                    self.activate_package(&session.package).await?;
                    self.finish(session, tools, automations, ctx).await?;
                    return Ok(serde_json::json!({ "ok": true, "verified": true }));
                }

                let is_setup_conversation = is_setup_conversation(&session, ctx);
                match message.trim().to_ascii_lowercase().as_str() {
                    _ if !is_setup_conversation
                        && channel == Some(session.package.name.as_str()) =>
                    {
                        send(tools, "Please enter the verification code.", ctx).await?;
                    }
                    _ if !is_setup_conversation => {}
                    "retry" => {
                        self.retry(session, tools, ctx).await?;
                    }
                    "continue" if activated => {
                        self.finish(session, tools, automations, ctx).await?;
                    }
                    "continue" => {
                        send(
                            tools,
                            "The channel has not started successfully yet. Reply retry to reconfigure it, or rollback to cancel setup.",
                            ctx,
                        )
                        .await?;
                    }
                    "rollback" | "cancel" => {
                        self.rollback(session, tools, automations, ctx).await?;
                    }
                    _ if activated => {
                        send(
                            tools,
                            &format!(
                                "I am waiting for a message from the {} channel containing the verification code I sent above. Reply retry to reconfigure, continue to skip verification, or rollback to cancel setup.",
                                session.package.name
                            ),
                            ctx,
                        )
                        .await?;
                    }
                    _ => {
                        send(
                            tools,
                            "Reply retry to reconfigure the channel, or rollback to cancel setup.",
                            ctx,
                        )
                        .await?;
                    }
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
        ctx: &ToolContext,
    ) -> Result<(), String> {
        let configs = required_config(&session.package);
        let Some(config) = configs.first() else {
            return self.complete(session, layout, tools, ctx).await;
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
        ctx: &ToolContext,
    ) -> Result<(), String> {
        write_package(layout, &session.package, &session.config_values)?;
        match self.activate_package(&session.package).await {
            Ok(()) => {
                let code = verification_code();
                let mut session = session;
                session.stage = InstallStage::Verify {
                    activated: true,
                    code: code.clone(),
                };
                self.save_session(session.clone());
                send(
                    tools,
                    &format!(
                        "The {} channel is installed and started. Send me this code from that channel to confirm it works: {}. You can also reply retry to reconfigure, continue to skip verification, or rollback to cancel setup.",
                        session.package.name,
                        code
                    ),
                    ctx,
                )
                .await
            }
            Err(err) => {
                let mut session = session;
                session.stage = InstallStage::Verify {
                    activated: false,
                    code: verification_code(),
                };
                self.save_session(session);
                send(
                    tools,
                    &format!(
                        "I installed the channel files, but the channel did not start: {err}. Reply retry to reconfigure it, or rollback to cancel setup."
                    ),
                    ctx,
                )
                .await
            }
        }
    }

    async fn retry(
        &self,
        mut session: InstallSession,
        tools: &InternalTools,
        ctx: &ToolContext,
    ) -> Result<(), String> {
        session.config_values.clear();
        if session.secrets.is_empty() {
            let configs = required_config(&session.package);
            if let Some(config) = configs.first() {
                session.stage = InstallStage::Config { index: 0 };
                self.save_session(session);
                return send(tools, config_prompt(config), ctx).await;
            }
            return self.complete(session, &self.setup.layout, tools, ctx).await;
        }
        session.stage = InstallStage::Secret { index: 0 };
        self.save_session(session.clone());
        send(
            tools,
            &secret_prompt(&session.package, &session.secrets[0]),
            ctx,
        )
        .await
    }

    async fn rollback(
        &self,
        session: InstallSession,
        tools: &InternalTools,
        automations: &Arc<Automations>,
        ctx: &ToolContext,
    ) -> Result<(), String> {
        let continuation = session.continuation.clone();
        self.setup
            .channel_runtime()?
            .remove_channel(&session.package.name)
            .await;
        remove_installed_package(&self.setup.layout, &session.package);
        *self.session.lock().expect("install channel session lock") = None;
        automations.delete(&format!("channel_auth:{}", session.package.name))?;
        if !automations.contains(HANDLE_CONVERSATION_AUTOMATION) {
            automations.add(Automations::handle_conversation())?;
        }
        send(
            tools,
            &format!(
                "I rolled back the {} channel setup. I will answer normally again.",
                session.package.name
            ),
            ctx,
        )
        .await?;
        publish_tool_result(
            continuation.as_ref(),
            serde_json::json!({ "ok": false, "channel": session.package.name, "rolled_back": true }),
        );
        Ok(())
    }

    async fn cancel(
        &self,
        session: InstallSession,
        tools: &InternalTools,
        automations: &Arc<Automations>,
        ctx: &ToolContext,
    ) -> Result<(), String> {
        let name = session.package.name.clone();
        let continuation = session.continuation.clone();
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
        .await?;
        publish_tool_result(
            continuation.as_ref(),
            serde_json::json!({ "ok": true, "channel": name, "cancelled": true }),
        );
        Ok(())
    }

    async fn finish(
        &self,
        session: InstallSession,
        tools: &InternalTools,
        automations: &Arc<Automations>,
        ctx: &ToolContext,
    ) -> Result<(), String> {
        let name = session.package.name.clone();
        let continuation = session.continuation.clone();
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
        .await?;
        publish_tool_result(
            continuation.as_ref(),
            serde_json::json!({ "ok": true, "channel": name, "installed": true }),
        );
        Ok(())
    }

    async fn activate_package(&self, package: &ChannelPackage) -> Result<(), String> {
        let runtime = self.setup.channel_runtime()?;
        runtime
            .activate_installed_channel(&package.name, package.manifest.external)
            .await
    }
}

fn setup_conversation(ctx: &ToolContext) -> Option<SetupConversation> {
    let conversation = ctx.conversation.as_ref()?;
    Some(SetupConversation {
        channel: conversation.channel.clone(),
        chat_id: conversation.chat_id.clone(),
        thread_id: conversation.thread_id.clone(),
    })
}

fn is_setup_conversation(session: &InstallSession, ctx: &ToolContext) -> bool {
    let Some(expected) = &session.setup_conversation else {
        return true;
    };
    let Some(conversation) = &ctx.conversation else {
        return false;
    };
    expected.channel == conversation.channel
        && expected.chat_id == conversation.chat_id
        && expected.thread_id == conversation.thread_id
}

impl RemoveChannelTool {
    pub fn new(setup: Arc<SetupServices>) -> Self {
        Self { setup }
    }

    async fn execute(
        &self,
        params: RemoveChannelParams,
        ctx: &ToolContext,
    ) -> Result<serde_json::Value, String> {
        let name = normalize_channel_name(&params.name)?;
        if BUILTIN_CHANNELS.contains(&name.as_str()) {
            return Err(format!(
                "the {name} channel is built in and cannot be removed"
            ));
        }

        let removed_files = remove_channel_files(&self.setup.layout, &name);
        let removed_runtime = self.setup.channel_runtime()?.remove_channel(&name).await;
        if !removed_files && !removed_runtime {
            send(
                &self.setup.tools,
                &format!("The {name} channel is not installed."),
                ctx,
            )
            .await?;
            return Ok(serde_json::json!({
                "ok": true,
                "channel": name,
                "removed": false,
            }));
        }

        send(
            &self.setup.tools,
            &format!(
                "The {name} channel has been removed. You can install it again whenever you want."
            ),
            ctx,
        )
        .await?;
        Ok(serde_json::json!({
            "ok": true,
            "channel": name,
            "removed": true,
        }))
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
        responder: ToolResponder,
    ) -> Result<(), String> {
        let output = ListChannelsTool::execute(self).await?;
        responder.respond(output)?;
        Ok(())
    }
}

#[async_trait]
impl InternalTool for RemoveChannelTool {
    fn name(&self) -> &str {
        "remove_channel"
    }

    fn tool_definition(&self) -> ToolDefinition {
        tools::tool_definition::<RemoveChannelParams>(
            self.name(),
            "Remove an installed Warden channel so it can be installed again. \
             Use this when the user asks to uninstall, remove, reset, or reinstall a channel.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
        responder: ToolResponder,
    ) -> Result<(), String> {
        let output = tools::execute_typed(args, |params| async {
            RemoveChannelTool::execute(self, params, ctx).await
        })
        .await?;
        responder.respond(output)?;
        Ok(())
    }
}

#[async_trait]
impl InternalTool for InstallChannelTool {
    fn name(&self) -> &str {
        "install_channel"
    }

    fn tool_definition(&self) -> ToolDefinition {
        tools::tool_definition::<InstallChannelStartParams>(
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
        responder: ToolResponder,
    ) -> Result<(), String> {
        let params: InstallChannelParams =
            serde_json::from_value(args).map_err(|err| err.to_string())?;
        let starts_setup = params.message.is_none() || self.active_session_name().is_none();
        let result = InstallChannelTool::execute(self, params, ctx, responder.clone()).await?;
        if starts_setup
            && result.get("ok").and_then(|ok| ok.as_bool()) == Some(true)
            && result
                .get("already_installed")
                .and_then(|value| value.as_bool())
                != Some(true)
        {
            return Ok(());
        }
        responder.respond(result)?;
        Ok(())
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

fn required_secrets(package: &ChannelPackage, secrets_store: &Secrets) -> Vec<SetupSecret> {
    let mut secrets = Vec::new();
    if package.manifest.external && !secrets_store.exists(NGROK_SECRET) {
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

fn secret_statuses(package: &ChannelPackage, secrets_store: &Secrets) -> Vec<serde_json::Value> {
    required_secrets(package, secrets_store)
        .into_iter()
        .map(|secret| {
            let secret_name = stored_secret_name(&secret.name);
            serde_json::json!({
                "name": secret.name,
                "secret": secret_name,
                "configured": secrets_store.exists(&secret_name),
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

fn verification_code() -> String {
    generate_secret(3).to_ascii_uppercase()
}

fn set_channel_owner_id(
    layout: &RuntimeLayout,
    package: &ChannelPackage,
    owner_id: &str,
) -> Result<(), String> {
    let config_path = layout
        .channels_dir()
        .join(format!("{}.config.json", package.name));
    let mut config: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&config_path)
            .map_err(|err| format!("could not read {}: {err}", config_path.display()))?,
    )
    .map_err(|err| format!("could not parse {}: {err}", config_path.display()))?;
    config["owner_id"] = serde_json::Value::String(owner_id.to_string());
    std::fs::write(
        &config_path,
        serde_json::to_string_pretty(&config).map_err(|err| err.to_string())?,
    )
    .map_err(|err| err.to_string())
}

fn write_package(
    layout: &RuntimeLayout,
    package: &ChannelPackage,
    config_values: &BTreeMap<String, serde_json::Value>,
) -> Result<(), String> {
    let channels_dir = layout.channels_dir();
    std::fs::create_dir_all(&channels_dir).map_err(|err| err.to_string())?;
    let wasm_dst = channels_dir.join(format!("{}.wasm", package.name));
    let config_dst = channels_dir.join(format!("{}.config.json", package.name));
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

fn installed_package_exists(layout: &RuntimeLayout, package: &ChannelPackage) -> bool {
    let channels_dir = layout.channels_dir();
    channels_dir.join(format!("{}.wasm", package.name)).exists()
        || channels_dir
            .join(format!("{}.config.json", package.name))
            .exists()
}

fn remove_installed_package(layout: &RuntimeLayout, package: &ChannelPackage) {
    remove_channel_files(layout, &package.name);
}

fn remove_channel_files(layout: &RuntimeLayout, name: &str) -> bool {
    let channels_dir = layout.channels_dir();
    let mut removed = false;
    for path in [
        channels_dir.join(format!("{name}.wasm")),
        channels_dir.join(format!("{name}.config.json")),
    ] {
        match std::fs::remove_file(&path) {
            Ok(()) => removed = true,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                eprintln!("failed to remove channel file {}: {err}", path.display());
            }
        }
    }
    removed
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
              "config": []
            }"#,
        )
        .unwrap();

        assert_eq!(manifest.name, "telegram");
        assert!(manifest.external);
        assert_eq!(manifest.secrets.len(), 2);
        assert!(manifest.config.is_empty());
    }
}
