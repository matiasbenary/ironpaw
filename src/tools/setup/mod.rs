pub mod install_channel;
pub mod install_common;
pub mod install_tool;
pub mod onboarding;
mod services;

use crate::{
    bus::Event,
    tools::{self, InternalTools, ToolCallContext, ToolContext},
};

pub use install_channel::{InstallChannelTool, ListChannelsTool, RemoveChannelTool};
pub use install_tool::{InstallToolTool, ListToolsTool};
pub use onboarding::OnboardingTool;
pub use services::SetupServices;

async fn send(tools: &InternalTools, content: &str, ctx: &ToolContext) -> Result<(), String> {
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

async fn delete_current_message(tools: &InternalTools, ctx: &ToolContext, label: &str) {
    let result = tools::run_tool(tools, "delete_message", serde_json::json!({}), ctx).await;
    if result.get("ok").and_then(|ok| ok.as_bool()) == Some(false) {
        eprintln!(
            "{label} could not delete inbound setup message: {}",
            result["error"].as_str().unwrap_or("unknown error")
        );
    }
}

fn publish_tool_result(
    bus: &crate::bus::Bus,
    continuation: Option<&ToolCallContext>,
    result: serde_json::Value,
) {
    let Some(continuation) = continuation else {
        return;
    };
    bus.publish(Event::ToolResult {
        origin: continuation.origin.clone(),
        tool_call_id: continuation.tool_call_id.clone(),
        tool_name: continuation.tool_name.clone(),
        result,
    });
}
