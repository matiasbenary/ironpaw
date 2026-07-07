pub mod install_channel;
pub mod install_common;
pub mod install_tool;
pub mod onboarding;
mod services;

use std::sync::Arc;

use crate::{
    automations::Automations,
    tools::{self, ExecutionContext, InternalTools, ToolResponder},
};

pub use install_channel::{
    InstallChannelTool, ListChannelsTool, RemoveChannelTool, SetupChannelTool,
};
pub use install_tool::{InstallToolTool, ListToolsTool};
pub use onboarding::OnboardingTool;
pub use services::SetupServices;

const HANDLE_CONVERSATION_AUTOMATION: &str = "handle-conversation";

fn pause_conversation(automations: &Arc<Automations>) -> Result<(), String> {
    automations.delete(HANDLE_CONVERSATION_AUTOMATION)
}

fn stop_setup_automation(automations: &Arc<Automations>, name: &str) -> Result<(), String> {
    automations.delete(name)?;
    if !automations.contains(HANDLE_CONVERSATION_AUTOMATION) {
        automations.add(Automations::handle_conversation())?;
    }
    Ok(())
}

pub(super) async fn send(
    tools: &InternalTools,
    content: &str,
    ctx: &ExecutionContext,
) -> Result<(), String> {
    let result = tools::run_tool(
        tools,
        "send_message",
        serde_json::json!({ "content": content }),
        ctx,
    )
    .await;
    if result.get("ok").and_then(|ok| ok.as_bool()) == Some(false) {
        return Err(result["error"]
            .as_str()
            .unwrap_or("send failed")
            .to_string());
    }
    Ok(())
}

pub(super) async fn delete_current_message(
    tools: &InternalTools,
    ctx: &ExecutionContext,
    message_id: Option<&str>,
    label: &str,
) {
    let Some(message_id) = message_id else {
        eprintln!("{label} could not delete inbound setup message: missing message_id");
        return;
    };
    let result = tools::run_tool(
        tools,
        "delete_message",
        serde_json::json!({ "message_id": message_id }),
        ctx,
    )
    .await;
    if result.get("ok").and_then(|ok| ok.as_bool()) == Some(false) {
        eprintln!(
            "{label} could not delete inbound setup message: {}",
            result["error"].as_str().unwrap_or("unknown error")
        );
    }
}

fn publish_tool_result(continuation: Option<&ToolResponder>, result: serde_json::Value) {
    let Some(continuation) = continuation else {
        return;
    };
    let _ = continuation.respond(result);
}
