use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;

#[path = "install_channel_package.rs"]
mod install_channel_package;
#[path = "install_channel_setup.rs"]
mod install_channel_setup;

use crate::{
    automations::{Automation, AutomationEffect, Trigger},
    bus::{ConversationKey, InvocationScope, SenderKind},
    config::RuntimeLayout,
    message::ToolDefinition,
    secrets::Secrets,
    tools::{
        self, ExecutionContext, InternalTool, ToolResponder,
        setup::{SetupServices, install_common::SetupSecret, pause_conversation, send},
    },
};

pub(super) const NGROK_SECRET: &str = "IRONPAW_NGROK_TOKEN";
use install_channel_package::{
    BUILTIN_CHANNELS, ChannelPackage, installed_package_exists, locate_channel_package,
    normalize_channel_name, remove_channel_files, required_secrets,
};

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct InstallChannelParams {
    /// Channel name to install, for example "telegram".
    pub name: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SetupChannelParams {
    /// Channel name being configured, for example "telegram".
    pub name: String,
    /// Channel that produced this setup reply.
    #[serde(default)]
    pub channel: Option<String>,
    /// Stable user id emitted by the channel that produced this setup reply.
    #[serde(default)]
    pub channel_user_id: Option<String>,
    /// Latest setup reply, filled by the temporary channel_auth automation.
    pub message: String,
    /// Provider message id for the follow-up reply, when available.
    #[serde(default)]
    pub message_id: Option<String>,
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
    setup_conversation: Option<ConversationKey>,
    continuation: Option<ToolResponder>,
}

#[derive(Clone)]
enum InstallStage {
    Confirm,
    Secret { index: usize },
    Config { index: usize },
    Verify { activated: bool, code: String },
}

pub struct InstallChannelTool {
    setup: Arc<SetupServices>,
    state: Arc<InstallChannelState>,
}

pub struct SetupChannelTool {
    setup: Arc<SetupServices>,
    state: Arc<InstallChannelState>,
}

struct InstallChannelState {
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
}

impl InstallChannelTool {
    pub fn new(setup: Arc<SetupServices>) -> Self {
        Self {
            setup,
            state: Arc::new(InstallChannelState {
                session: Mutex::new(None),
            }),
        }
    }

    pub fn setup_tool(&self) -> SetupChannelTool {
        SetupChannelTool {
            setup: Arc::clone(&self.setup),
            state: Arc::clone(&self.state),
        }
    }

    fn automation(
        channel_name: &str,
        sender: SenderKind,
        conversation: Option<ConversationKey>,
    ) -> Automation {
        Automation {
            name: format!(
                "channel_auth:{channel_name}:{}",
                match sender {
                    SenderKind::User => "user",
                    SenderKind::Guest => "guest",
                    SenderKind::System => "system",
                }
            ),
            trigger: Trigger::MessageIn {
                sender,
                conversation,
            },
            effect: AutomationEffect::Tool {
                tool: "setup_channel".to_string(),
                args: serde_json::json!({
                    "name": channel_name,
                    "channel": "{{event.channel}}",
                    "channel_user_id": "{{event.channel_user_id}}",
                    "message": "{{event.text}}",
                    "message_id": "{{event.message_id}}",
                }),
            },
        }
    }

    async fn execute(
        &self,
        params: InstallChannelParams,
        ctx: &ExecutionContext,
        responder: ToolResponder,
    ) -> Result<serde_json::Value, String> {
        let name = normalize_channel_name(&params.name)?;
        self.start(&name, ctx, responder).await
    }

    async fn start(
        &self,
        name: &str,
        ctx: &ExecutionContext,
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
        let user_conversation = match &ctx.invoked_by {
            InvocationScope::Conversation(conversation) => Some(conversation.clone()),
            InvocationScope::ScheduledAutomation { .. } => None,
        };
        *self
            .state
            .session
            .lock()
            .expect("install channel session lock") = Some(InstallSession {
            package,
            secrets,
            stage: InstallStage::Confirm,
            config_values: BTreeMap::new(),
            setup_conversation: setup_conversation(ctx),
            continuation: Some(responder),
        });
        pause_conversation(automations)?;
        automations.add(Self::automation(name, SenderKind::User, user_conversation))?;
        super::request_choice(
            tools,
            &format!("Install the {name} channel?"),
            &[("Install", "yes"), ("Cancel", "no")],
            ctx,
        )
        .await?;
        Ok(serde_json::json!({
            "ok": true,
            "channel": name,
            "message": "deterministic channel setup prompt sent"
        }))
    }
}

fn setup_conversation(ctx: &ExecutionContext) -> Option<ConversationKey> {
    match &ctx.invoked_by {
        InvocationScope::Conversation(conversation) => Some(conversation.clone()),
        InvocationScope::ScheduledAutomation { .. } => None,
    }
}

fn is_setup_conversation(session: &InstallSession, ctx: &ExecutionContext) -> bool {
    let Some(expected) = &session.setup_conversation else {
        return true;
    };
    let conversation = match &ctx.invoked_by {
        InvocationScope::Conversation(conversation) => conversation,
        InvocationScope::ScheduledAutomation { .. } => return false,
    };
    expected == conversation
}

impl RemoveChannelTool {
    pub fn new(setup: Arc<SetupServices>) -> Self {
        Self { setup }
    }

    async fn execute(
        &self,
        params: RemoveChannelParams,
        ctx: &ExecutionContext,
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
            "List Ironpaw channels, including installed runtime channels and \
             installable local channel packages. Use this before install_channel \
             when the user asks what channels are available.",
        )
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ExecutionContext,
        responder: ToolResponder,
    ) {
        responder.respond(
            ListChannelsTool::execute(self)
                .await
                .map_err(crate::tools::ToolError::new),
        );
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
            "Remove an installed Ironpaw channel so it can be installed again. \
             Use this when the user asks to uninstall, remove, reset, or reinstall a channel.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ExecutionContext,
        responder: ToolResponder,
    ) {
        responder.respond(
            tools::execute_typed(args, |params| async {
                RemoveChannelTool::execute(self, params, ctx).await
            })
            .await,
        );
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
        ctx: &ExecutionContext,
        responder: ToolResponder,
    ) {
        let result = async {
            let params = serde_json::from_value(args).map_err(|err| err.to_string())?;
            InstallChannelTool::execute(self, params, ctx, responder.clone()).await
        }
        .await;
        match result {
            Ok(result) if result["ok"] == true && result["already_installed"] != true => {}
            Ok(result) => responder.respond(Ok(result)),
            Err(error) => responder.respond(Err(crate::tools::ToolError::new(error))),
        }
    }
}

#[async_trait]
impl InternalTool for SetupChannelTool {
    fn name(&self) -> &str {
        "setup_channel"
    }

    fn tool_definition(&self) -> ToolDefinition {
        tools::tool_definition::<SetupChannelParams>(
            self.name(),
            "Continue the deterministic setup flow for a channel installation. \
             This is called by temporary setup automations with inbound user replies.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ExecutionContext,
        responder: ToolResponder,
    ) {
        let result = async {
            let params: SetupChannelParams =
                serde_json::from_value(args).map_err(|err| err.to_string())?;
            let name = normalize_channel_name(&params.name)?;
            self.continue_setup(
                &name,
                params.channel.as_deref(),
                params.channel_user_id.as_deref(),
                &params.message,
                params.message_id.as_deref(),
                ctx,
            )
            .await
        }
        .await;
        responder.respond(result.map_err(crate::tools::ToolError::new));
    }
}
