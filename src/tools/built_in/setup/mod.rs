pub mod install_channel;
pub mod install_common;
pub mod install_tool;
pub mod onboarding;
mod services;

use std::sync::{Arc, Mutex};

use crate::{
    automations::Automations,
    bus::{ConversationKey, MessageRef},
    tools::{self, ExecutionContext, InternalTools, ToolResponder},
};

pub use install_channel::{
    InstallChannelTool, ListChannelsTool, RemoveChannelTool, SetupChannelTool,
};
pub use install_tool::{ConfigureToolTool, InstallToolTool, ListToolsTool};
pub use onboarding::OnboardingTool;
pub use services::SetupServices;

pub(super) struct SessionState<T>(Mutex<Option<T>>);

impl<T> SessionState<T> {
    pub fn new() -> Self {
        Self(Mutex::new(None))
    }

    pub fn save(&self, session: T) {
        *self.0.lock().expect("setup session lock") = Some(session);
    }

    pub fn take(&self) -> Option<T> {
        self.0.lock().expect("setup session lock").take()
    }
}

impl<T: Clone> SessionState<T> {
    pub fn get(&self) -> Option<T> {
        self.0.lock().expect("setup session lock").clone()
    }
}

fn stop_setup_automation(automations: &Arc<Automations>, name: &str) -> Result<(), String> {
    if automations.remove_runtime(name) {
        Ok(())
    } else {
        Err(format!("unknown automation: {name}"))
    }
}

pub(super) async fn send(
    tools: &InternalTools,
    conversation: &ConversationKey,
    content: &str,
    ctx: &ExecutionContext,
) -> Result<(), String> {
    let result = tools::run_tool(
        tools,
        "send_message",
        serde_json::json!({
            "channel": conversation.channel,
            "chat_id": conversation.chat_id,
            "thread_id": conversation.thread_id,
            "content": content,
        }),
        ctx,
    )
    .await;
    result.map(|_| ()).map_err(|error| error.message)
}

pub(super) async fn request_choice(
    tools: &InternalTools,
    conversation: &ConversationKey,
    question: &str,
    options: &[(&str, &str)],
    ctx: &ExecutionContext,
) -> Result<(), String> {
    let result = tools::run_tool(
        tools,
        "request_user_input",
        serde_json::json!({
            "conversation": conversation,
            "question": question,
            "options": options
                .iter()
                .map(|(label, value)| serde_json::json!({ "label": label, "value": value }))
                .collect::<Vec<_>>(),
        }),
        ctx,
    )
    .await;
    result.map(|_| ()).map_err(|error| error.message)
}

pub(super) async fn delete_current_message(
    tools: &InternalTools,
    conversation: &ConversationKey,
    ctx: &ExecutionContext,
    message_id: Option<&str>,
    label: &str,
) {
    let Some(message_id) = message_id else {
        eprintln!("{label} could not delete inbound setup message: missing message_id");
        return;
    };
    if let Err(error) = tools.history.delete_external_message(&MessageRef {
        conversation: conversation.clone(),
        message_id: message_id.to_string(),
    }) {
        eprintln!("{label} could not delete inbound setup history: {error}");
    }
    let result = tools::run_tool(
        tools,
        "delete_message",
        serde_json::json!({
            "channel": conversation.channel,
            "chat_id": conversation.chat_id,
            "thread_id": conversation.thread_id,
            "message_id": message_id,
        }),
        ctx,
    )
    .await;
    if let Err(error) = result {
        eprintln!(
            "{label} could not delete inbound setup message: {}",
            error.message
        );
    }
}

fn publish_tool_result(continuation: Option<&ToolResponder>, result: crate::tools::ToolResult) {
    let Some(continuation) = continuation else {
        return;
    };
    continuation.respond(result);
}
