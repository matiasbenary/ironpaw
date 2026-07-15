use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;

#[path = "install_tool_package.rs"]
mod install_tool_package;

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
            install_common::{
                SetupSecret, config_prompt, config_value, is_yes, secret_value, stored_secret_name,
            },
            pause_conversation, publish_tool_result, send, stop_setup_automation,
        },
        wasm::WasmTool,
    },
};

pub use install_tool_package::install_packaged_tool;
use install_tool_package::{
    ToolPackage, install_package, locate_tool_package, normalize_tool_name, required_config,
    required_secrets,
};

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct InstallToolParams {
    /// Tool name to install, for example "web_search".
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

pub struct InstallToolTool {
    setup: Arc<SetupServices>,
    state: Arc<InstallToolState>,
}

pub struct SetupToolTool {
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
        let package = locate_tool_package(&self.setup.layout, name)?;
        *self
            .state
            .session
            .lock()
            .expect("install tool session lock") = Some(InstallSession {
            package,
            stage: InstallStage::Confirm,
            config_values: BTreeMap::new(),
            continuation: Some(responder),
        });
        pause_conversation(&self.setup.automations)?;
        self.setup
            .automations
            .add(Self::automation(name, conversation))?;
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
        if session.package.name != name {
            return Err(format!(
                "tool setup is active for '{}', not '{name}'",
                session.package.name
            ));
        }

        match session.stage.clone() {
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
                delete_current_message(&self.setup.tools, ctx, message_id, "install_tool").await;

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
        *self
            .state
            .session
            .lock()
            .expect("install tool session lock") = Some(session);
    }

    async fn prompt_config_or_finish(
        &self,
        mut session: InstallSession,
        ctx: &ExecutionContext,
    ) -> Result<(), String> {
        let configs = required_config(&session.package);
        let Some(config) = configs.first() else {
            return self.complete(session, ctx).await;
        };
        session.stage = InstallStage::Config { index: 0 };
        self.save_session(session);
        send(&self.setup.tools, config_prompt(config), ctx).await
    }

    async fn complete(
        &self,
        session: InstallSession,
        ctx: &ExecutionContext,
    ) -> Result<(), String> {
        let manifest =
            install_package(&self.setup.layout, &session.package, &session.config_values)?;
        let tool = WasmTool::from_manifest(manifest, self.setup.secrets.tools.clone())?;
        self.setup.tools.registry.add(Arc::new(tool))?;
        self.finish(session, ctx).await
    }

    async fn cancel(&self, session: InstallSession, ctx: &ExecutionContext) -> Result<(), String> {
        let name = session.package.name.clone();
        let continuation = session.continuation.clone();
        *self
            .state
            .session
            .lock()
            .expect("install tool session lock") = None;
        stop_setup_automation(&self.setup.automations, &format!("tool_auth:{name}"))?;
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
            serde_json::json!({ "ok": false, "tool": name, "error": error }),
        );
        send(
            &self.setup.tools,
            &format!("The {name} tool could not be installed: {error}"),
            ctx,
        )
        .await
    }

    async fn finish(&self, session: InstallSession, ctx: &ExecutionContext) -> Result<(), String> {
        let name = session.package.name.clone();
        let continuation = session.continuation.clone();
        *self
            .state
            .session
            .lock()
            .expect("install tool session lock") = None;
        stop_setup_automation(&self.setup.automations, &format!("tool_auth:{name}"))?;
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
        _ctx: &ExecutionContext,
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
        ctx: &ExecutionContext,
        responder: ToolResponder,
    ) -> Result<(), String> {
        let params: InstallToolParams =
            serde_json::from_value(args).map_err(|err| err.to_string())?;
        let result = InstallToolTool::execute(self, params, ctx, responder.clone()).await?;
        if result.get("ok").and_then(|ok| ok.as_bool()) == Some(true) {
            return Ok(());
        }
        responder.respond(result)?;
        Ok(())
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
            "Continue the deterministic setup flow for a tool installation. \
             This is called by temporary setup automations with inbound user replies.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ExecutionContext,
        responder: ToolResponder,
    ) -> Result<(), String> {
        let params: SetupToolParams =
            serde_json::from_value(args).map_err(|err| err.to_string())?;
        let name = normalize_tool_name(&params.name)?;
        let result = match self
            .continue_setup(&name, &params.message, params.message_id.as_deref(), ctx)
            .await
        {
            Ok(result) => result,
            Err(error) => {
                self.fail(&name, &error, ctx).await?;
                serde_json::json!({ "ok": false, "error": error })
            }
        };
        responder.respond(result)?;
        Ok(())
    }
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
