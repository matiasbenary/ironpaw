use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;

#[path = "install_tool_package.rs"]
mod install_tool_package;
#[path = "installable_tool.rs"]
mod installable_tool;

use crate::{
    automations::{Automation, AutomationEffect, Trigger},
    bus::{ConversationKey, InvocationScope, SenderKind},
    config::RuntimeLayout,
    message::ToolDefinition,
    secrets::Secrets,
    tools::{
        self, ExecutionContext, InternalTool, ToolResponder,
        setup::{
            SetupServices, delete_current_message,
            install_common::{SetupSecret, config_prompt, config_value, is_yes, secret_value},
            pause_conversation, publish_tool_result, request_choice, send, stop_setup_automation,
        },
        wasm::WasmTool,
    },
};

pub use install_tool_package::install_packaged_tool;
use install_tool_package::{
    install_package, installed_tool_manifest, normalize_tool_name, reconfigure_package,
};
use installable_tool::InstallableTool;

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct InstallToolParams {
    /// Tool name to install, for example "web_search".
    pub name: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ConfigureToolParams {
    /// Installed tool name, for example "web_search".
    pub name: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SetupToolParams {
    /// Tool name being configured.
    pub name: String,
    /// Latest setup reply, filled by the temporary tool_auth automation.
    pub message: String,
    /// Provider message id for the follow-up reply, when available.
    #[serde(default)]
    pub message_id: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ListToolsParams {}

#[derive(Clone)]
struct InstallSession {
    package: InstallableTool,
    mode: SetupMode,
    stage: InstallStage,
    config_values: BTreeMap<String, serde_json::Value>,
    secret_values: BTreeMap<String, String>,
    continuation: Option<ToolResponder>,
}

#[derive(Clone)]
enum InstallStage {
    ConfirmInstall,
    ConfirmSecret { index: usize },
    Secret { index: usize },
    ConfirmConfig { index: usize },
    Config { index: usize },
}

#[derive(Clone)]
enum SetupMode {
    Install,
    Configure { current_config: serde_json::Value },
}

pub struct InstallToolTool {
    setup: Arc<SetupServices>,
    state: Arc<InstallToolState>,
}

pub struct SetupToolTool {
    setup: Arc<SetupServices>,
    state: Arc<InstallToolState>,
}

pub struct ConfigureToolTool {
    setup: Arc<SetupServices>,
    state: Arc<InstallToolState>,
}

struct InstallToolState {
    session: Mutex<Option<InstallSession>>,
}

pub struct ListToolsTool {
    layout: RuntimeLayout,
    secrets: Secrets,
}

impl ListToolsTool {
    pub fn new(layout: RuntimeLayout, secrets: Secrets) -> Self {
        Self { layout, secrets }
    }
}

impl InstallToolTool {
    pub fn new(setup: Arc<SetupServices>) -> Self {
        Self {
            setup,
            state: Arc::new(InstallToolState {
                session: Mutex::new(None),
            }),
        }
    }

    pub fn setup_tool(&self) -> SetupToolTool {
        SetupToolTool {
            setup: Arc::clone(&self.setup),
            state: Arc::clone(&self.state),
        }
    }

    pub fn configure_tool(&self) -> ConfigureToolTool {
        ConfigureToolTool {
            setup: Arc::clone(&self.setup),
            state: Arc::clone(&self.state),
        }
    }

    fn automation(tool_name: &str, conversation: &ConversationKey) -> Automation {
        Automation {
            name: format!("tool_auth:{tool_name}"),
            trigger: Trigger::MessageIn {
                sender: SenderKind::User,
                conversation: Some(conversation.clone()),
            },
            effect: AutomationEffect::Tool {
                tool: "setup_tool".to_string(),
                args: serde_json::json!({
                    "name": tool_name,
                    "message": "{{event.text}}",
                    "message_id": "{{event.message_id}}",
                }),
            },
        }
    }

    async fn execute(
        &self,
        params: InstallToolParams,
        ctx: &ExecutionContext,
        responder: ToolResponder,
    ) -> Result<serde_json::Value, String> {
        let name = normalize_tool_name(&params.name)?;
        self.start(&name, ctx, responder).await
    }

    async fn start(
        &self,
        name: &str,
        ctx: &ExecutionContext,
        responder: ToolResponder,
    ) -> Result<serde_json::Value, String> {
        let conversation = match &ctx.invoked_by {
            InvocationScope::Conversation(conversation) => conversation,
            InvocationScope::ScheduledAutomation { automation_id } => {
                return Err(format!(
                    "install_tool cannot start from scheduled automation '{automation_id}'"
                ));
            }
        };
        let package = InstallableTool::locate(&self.setup.layout, name)?;
        let approval_prompt = package.approval_prompt();
        *self
            .state
            .session
            .lock()
            .expect("install tool session lock") = Some(InstallSession {
            package,
            mode: SetupMode::Install,
            stage: InstallStage::ConfirmInstall,
            config_values: BTreeMap::new(),
            secret_values: BTreeMap::new(),
            continuation: Some(responder),
        });
        pause_conversation(&self.setup.automations)?;
        self.setup
            .automations
            .add(Self::automation(name, conversation))?;
        request_choice(
            &self.setup.tools,
            &approval_prompt,
            &[("Install", "yes"), ("Cancel", "no")],
            ctx,
        )
        .await?;
        Ok(serde_json::json!({
            "ok": true,
            "tool": name,
            "message": "deterministic tool setup prompt sent"
        }))
    }

    async fn configure(
        &self,
        params: ConfigureToolParams,
        ctx: &ExecutionContext,
        responder: ToolResponder,
    ) -> Result<serde_json::Value, String> {
        let name = normalize_tool_name(&params.name)?;
        let conversation = match &ctx.invoked_by {
            InvocationScope::Conversation(conversation) => conversation,
            InvocationScope::ScheduledAutomation { automation_id } => {
                return Err(format!(
                    "configure_tool cannot start from scheduled automation '{automation_id}'"
                ));
            }
        };
        let package = InstallableTool::locate(&self.setup.layout, &name)?;
        let InstallableTool::Wasm(_) = &package else {
            return Err("shell tools do not have configurable non-secret settings".to_string());
        };
        if !self.setup.tools.registry.contains(&name) {
            return Err(format!("tool '{name}' is not installed"));
        }
        let manifest = installed_tool_manifest(&self.setup.layout, &name)?
            .ok_or_else(|| format!("tool '{name}' is not installed"))?;
        let current_config =
            serde_json::from_str(&manifest.config_json).map_err(|err| err.to_string())?;
        *self
            .state
            .session
            .lock()
            .expect("install tool session lock") = Some(InstallSession {
            package,
            mode: SetupMode::Configure { current_config },
            stage: InstallStage::ConfirmSecret { index: 0 },
            config_values: BTreeMap::new(),
            secret_values: BTreeMap::new(),
            continuation: Some(responder),
        });
        pause_conversation(&self.setup.automations)?;
        self.setup
            .automations
            .add(Self::automation(&name, conversation))?;
        let session = self
            .state
            .session
            .lock()
            .expect("install tool session lock")
            .clone()
            .expect("configure session");
        self.setup_tool()
            .prompt_secret_or_finish(session, 0, ctx)
            .await?;
        Ok(serde_json::json!({
            "tool": name,
            "message": "deterministic tool configuration prompt sent"
        }))
    }
}

impl SetupToolTool {
    async fn continue_setup(
        &self,
        name: &str,
        message: &str,
        message_id: Option<&str>,
        ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, String> {
        let session = self
            .state
            .session
            .lock()
            .expect("install tool session lock")
            .clone()
            .ok_or_else(|| "tool setup is not active; ask to install the tool again".to_string())?;
        if session.package.name() != name {
            return Err(format!(
                "tool setup is active for '{}', not '{name}'",
                session.package.name()
            ));
        }

        match session.stage.clone() {
            InstallStage::ConfirmInstall => {
                if !is_yes(message) {
                    self.cancel(session, ctx).await?;
                    return Ok(serde_json::json!({ "ok": true, "cancelled": true }));
                }
                self.prompt_secret_or_finish(session, 0, ctx).await?;
            }
            InstallStage::ConfirmSecret { index } => {
                if is_yes(message) {
                    let prompt =
                        secret_prompt(&session.package, &session.package.required_secrets()[index]);
                    let mut session = session;
                    session.stage = InstallStage::Secret { index };
                    self.save_session(session);
                    send(&self.setup.tools, &prompt, ctx).await?;
                } else {
                    self.prompt_secret_or_finish(session, index + 1, ctx)
                        .await?;
                }
            }
            InstallStage::Secret { index } => {
                let mut session = session;
                let secrets = session.package.required_secrets();
                let secret = &secrets[index];
                let value = secret_value(secret, message)?;
                session.secret_values.insert(secret.name.clone(), value);
                delete_current_message(&self.setup.tools, ctx, message_id, "install_tool").await;
                self.prompt_secret_or_finish(session, index + 1, ctx)
                    .await?;
            }
            InstallStage::ConfirmConfig { index } => {
                if is_yes(message) {
                    let prompt =
                        config_prompt(&session.package.required_config()[index]).to_string();
                    let mut session = session;
                    session.stage = InstallStage::Config { index };
                    self.save_session(session);
                    send(&self.setup.tools, &prompt, ctx).await?;
                } else {
                    self.prompt_config_or_finish(session, index + 1, ctx)
                        .await?;
                }
            }
            InstallStage::Config { index } => {
                let mut session = session;
                let configs = session.package.required_config();
                let config = &configs[index];
                session
                    .config_values
                    .insert(config.path.clone(), config_value(config, message)?);

                self.prompt_config_or_finish(session, index + 1, ctx)
                    .await?;
            }
        }

        Ok(serde_json::json!({ "ok": true }))
    }

    fn save_session(&self, session: InstallSession) {
        *self
            .state
            .session
            .lock()
            .expect("install tool session lock") = Some(session);
    }

    async fn prompt_secret_or_finish(
        &self,
        mut session: InstallSession,
        index: usize,
        ctx: &ExecutionContext,
    ) -> Result<(), String> {
        let secrets = session.package.required_secrets();
        let Some(secret) = secrets.get(index) else {
            return self.prompt_config_or_finish(session, 0, ctx).await;
        };
        if matches!(session.mode, SetupMode::Configure { .. })
            && self
                .setup
                .secrets
                .exists(&session.package.secret_store_name(&secret.name))
        {
            session.stage = InstallStage::ConfirmSecret { index };
            self.save_session(session);
            return request_choice(
                &self.setup.tools,
                &format!(
                    "{}: a value is already saved for {}. Do you want to change it?",
                    secret.name, secret.name
                ),
                &[("Change", "yes"), ("Keep", "no")],
                ctx,
            )
            .await;
        }
        let prompt = secret_prompt(&session.package, secret);
        session.stage = InstallStage::Secret { index };
        self.save_session(session);
        send(&self.setup.tools, &prompt, ctx).await
    }

    async fn prompt_config_or_finish(
        &self,
        mut session: InstallSession,
        index: usize,
        ctx: &ExecutionContext,
    ) -> Result<(), String> {
        let configs = session.package.required_config();
        let Some(config) = configs.get(index) else {
            return self.complete(session, ctx).await;
        };
        if let SetupMode::Configure { current_config } = &session.mode
            && let Some(value) = current_config.pointer(&config.path)
        {
            let prompt = format!(
                "{} currently has value {}. Do you want to change it?",
                config_prompt(config),
                value
            );
            session.stage = InstallStage::ConfirmConfig { index };
            self.save_session(session);
            return request_choice(
                &self.setup.tools,
                &prompt,
                &[("Change", "yes"), ("Keep", "no")],
                ctx,
            )
            .await;
        }
        let prompt = config_prompt(config).to_string();
        session.stage = InstallStage::Config { index };
        self.save_session(session);
        send(&self.setup.tools, &prompt, ctx).await
    }

    async fn complete(
        &self,
        session: InstallSession,
        ctx: &ExecutionContext,
    ) -> Result<(), String> {
        let configured = matches!(&session.mode, SetupMode::Configure { .. });
        match &session.package {
            InstallableTool::Wasm(package) => {
                let manifest = if configured {
                    reconfigure_package(&self.setup.layout, package, &session.config_values)?
                } else {
                    install_package(&self.setup.layout, package, &session.config_values)?
                };
                save_secrets(&self.setup.secrets, &session)?;
                let tool = WasmTool::from_manifest(manifest, self.setup.secrets.tools.clone())?;
                if configured {
                    self.setup.tools.registry.replace(Arc::new(tool))?;
                } else {
                    self.setup
                        .tools
                        .registry
                        .add(Arc::new(tool), crate::tools::ToolVisibility::Agent)?;
                }
            }
            InstallableTool::Shell(package) => {
                if configured {
                    return Err("shell tools cannot be reconfigured".to_string());
                }
                self.setup
                    .tools
                    .shell_executor
                    .install(
                        Arc::new(package.manifest.clone()),
                        package.source_dir.clone(),
                    )
                    .await?;
                save_secrets(&self.setup.secrets, &session)?;
            }
        }
        self.setup.tool_state.enable(session.package.name())?;
        self.finish(session, ctx).await
    }

    async fn cancel(&self, session: InstallSession, ctx: &ExecutionContext) -> Result<(), String> {
        let name = session.package.name().to_string();
        let continuation = session.continuation.clone();
        *self
            .state
            .session
            .lock()
            .expect("install tool session lock") = None;
        stop_setup_automation(&self.setup.automations, &format!("tool_auth:{name}"))?;
        send(
            &self.setup.tools,
            if matches!(session.mode, SetupMode::Install) {
                "Tool installation cancelled. I will answer normally again."
            } else {
                "Tool configuration cancelled. I will answer normally again."
            },
            ctx,
        )
        .await?;
        publish_tool_result(
            continuation.as_ref(),
            Ok(serde_json::json!({ "tool": name, "cancelled": true })),
        );
        Ok(())
    }

    async fn fail(&self, name: &str, error: &str, ctx: &ExecutionContext) -> Result<(), String> {
        let session = self
            .state
            .session
            .lock()
            .expect("install tool session lock")
            .take();
        stop_setup_automation(&self.setup.automations, &format!("tool_auth:{name}"))?;
        publish_tool_result(
            session
                .as_ref()
                .and_then(|session| session.continuation.as_ref()),
            Err(crate::tools::ToolError::new(error)),
        );
        send(
            &self.setup.tools,
            &format!("The {name} tool could not be configured: {error}"),
            ctx,
        )
        .await
    }

    async fn finish(&self, session: InstallSession, ctx: &ExecutionContext) -> Result<(), String> {
        let name = session.package.name().to_string();
        let configured = matches!(session.mode, SetupMode::Configure { .. });
        let continuation = session.continuation.clone();
        *self
            .state
            .session
            .lock()
            .expect("install tool session lock") = None;
        stop_setup_automation(&self.setup.automations, &format!("tool_auth:{name}"))?;
        send(
            &self.setup.tools,
            &format!(
                "The {name} tool is {}.",
                if configured {
                    "configured"
                } else {
                    "installed"
                }
            ),
            ctx,
        )
        .await?;
        let result = if configured {
            serde_json::json!({ "tool": name, "configured": true })
        } else {
            serde_json::json!({ "tool": name, "installed": true })
        };
        publish_tool_result(continuation.as_ref(), Ok(result));
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
            "List installed and installable Ironpaw WASM and shell tools, including \
             each tool's setup requirements. Use this before attempting a capability \
             that is not already provided by a callable tool.",
        )
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ExecutionContext,
        responder: ToolResponder,
    ) {
        responder.respond(
            ListToolsTool::execute(self)
                .await
                .map_err(crate::tools::ToolError::new),
        );
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
            "Install a local WASM or shell tool package. When called with only \
             `name`, this starts a deterministic setup flow that asks the user \
             for confirmation and required secrets without exposing those \
             secret replies to the LLM.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ExecutionContext,
        responder: ToolResponder,
    ) {
        let result = async {
            let params = serde_json::from_value(args).map_err(|err| err.to_string())?;
            InstallToolTool::execute(self, params, ctx, responder.clone()).await
        }
        .await;
        match result {
            Ok(result) if result["ok"] == true => {}
            Ok(result) => responder.respond(Ok(result)),
            Err(error) => responder.respond(Err(crate::tools::ToolError::new(error))),
        }
    }
}

#[async_trait]
impl InternalTool for ConfigureToolTool {
    fn name(&self) -> &str {
        "configure_tool"
    }

    fn tool_definition(&self) -> ToolDefinition {
        tools::tool_definition::<ConfigureToolParams>(
            self.name(),
            "Reconfigure an installed external WASM tool. This starts a deterministic setup flow \
             that asks before replacing each saved secret or non-secret configuration value.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ExecutionContext,
        responder: ToolResponder,
    ) {
        let result = async {
            let params = serde_json::from_value(args).map_err(|err| err.to_string())?;
            InstallToolTool {
                setup: Arc::clone(&self.setup),
                state: Arc::clone(&self.state),
            }
            .configure(params, ctx, responder.clone())
            .await
        }
        .await;
        match result {
            Ok(_) => {}
            Err(error) => responder.respond(Err(crate::tools::ToolError::new(error))),
        }
    }
}

#[async_trait]
impl InternalTool for SetupToolTool {
    fn name(&self) -> &str {
        "setup_tool"
    }

    fn tool_definition(&self) -> ToolDefinition {
        tools::tool_definition::<SetupToolParams>(
            self.name(),
            "Continue the deterministic setup flow for a tool installation or configuration. \
             This is called by temporary setup automations with inbound user replies.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ExecutionContext,
        responder: ToolResponder,
    ) {
        let result: Result<serde_json::Value, String> = async {
            let params: SetupToolParams =
                serde_json::from_value(args).map_err(|err| err.to_string())?;
            let name = normalize_tool_name(&params.name)?;
            Ok(
                match self
                    .continue_setup(&name, &params.message, params.message_id.as_deref(), ctx)
                    .await
                {
                    Ok(result) => result,
                    Err(error) => {
                        self.fail(&name, &error, ctx).await?;
                        serde_json::json!({ "error": error })
                    }
                },
            )
        }
        .await;
        responder.respond(result.map_err(crate::tools::ToolError::new));
    }
}

fn save_secrets(secrets: &Secrets, session: &InstallSession) -> Result<(), String> {
    for (name, value) in &session.secret_values {
        secrets.set(&session.package.secret_store_name(name), value)?;
    }
    Ok(())
}

fn secret_prompt(package: &InstallableTool, secret: &SetupSecret) -> String {
    let mut prompt = format!(
        "{}: {}",
        package.display_name(),
        secret.prompt.as_deref().unwrap_or(&secret.name)
    );
    if let Some(setup_url) = package.setup_url() {
        prompt.push_str(&format!(" Setup: {setup_url}"));
    }
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_hyphenated_tool_name() {
        assert_eq!(normalize_tool_name("web-search").unwrap(), "web_search");
    }

    #[test]
    fn setup_automation_is_scoped_to_starting_conversation() {
        let conversation = ConversationKey {
            channel: "telegram".to_string(),
            chat_id: "chat-1".to_string(),
            thread_id: Some("thread-1".to_string()),
        };

        let automation = InstallToolTool::automation("web_search", &conversation);

        let Trigger::MessageIn {
            sender,
            conversation: trigger_conversation,
        } = automation.trigger
        else {
            panic!("expected message_in trigger");
        };
        assert_eq!(sender, SenderKind::User);
        assert_eq!(trigger_conversation, Some(conversation));
        let AutomationEffect::Tool { tool, args } = automation.effect else {
            panic!("expected tool effect");
        };
        assert_eq!(tool, "setup_tool");
        assert_eq!(args["name"], "web_search");
        assert_eq!(args["message"], "{{event.text}}");
        assert_eq!(args["message_id"], "{{event.message_id}}");
    }
}
