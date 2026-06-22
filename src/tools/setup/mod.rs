pub mod install_channel;
pub mod onboarding;
mod services;

use crate::tools::{self, InternalTools, ToolContext};

pub use install_channel::{InstallChannelTool, ListChannelsTool};
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
