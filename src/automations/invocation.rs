use crate::{
    bus::{Event, InvocationScope, Sender},
    tools::ExecutionContext,
};

pub(super) fn from_bus(event: &Event) -> Option<(ExecutionContext, serde_json::Value)> {
    match event {
        Event::MessageIn {
            sender,
            conversation,
            message_id,
            message,
        } => {
            let text = match message {
                crate::message::Message::User { text, .. } => text.clone(),
                _ => String::new(),
            };
            Some((
                ExecutionContext::new(InvocationScope::Conversation(conversation.clone())),
                serde_json::json!({
                    "type": "message_in",
                    "channel": &conversation.channel,
                    "sender": sender_name(sender),
                    "channel_user_id": sender.channel_user_id(),
                    "conversation": conversation,
                    "chat_id": &conversation.chat_id,
                    "thread_id": &conversation.thread_id,
                    "message_id": message_id,
                    "text": text,
                }),
            ))
        }
        Event::ToolResult {
            context,
            tool_call_id,
            tool_name,
            result,
            ..
        } => {
            let conversation = match &context.invoked_by {
                InvocationScope::Conversation(conversation) => Some(conversation),
                InvocationScope::ScheduledAutomation { .. } => None,
            };
            Some((
                context.clone(),
                serde_json::json!({
                    "type": "tool_result",
                    "channel": conversation.map(|conversation| &conversation.channel),
                    "conversation": conversation,
                    "chat_id": conversation.map(|conversation| &conversation.chat_id),
                    "thread_id": conversation.and_then(|conversation| conversation.thread_id.as_ref()),
                    "tool_call_id": tool_call_id,
                    "tool_name": tool_name,
                    "result": result,
                }),
            ))
        }
        Event::ApiKeyError {
            conversation,
            error,
        } => Some((
            ExecutionContext::new(InvocationScope::Conversation(conversation.clone())),
            serde_json::json!({
                "type": "api_key_error",
                "channel": &conversation.channel,
                "conversation": conversation,
                "chat_id": &conversation.chat_id,
                "thread_id": &conversation.thread_id,
                "error": error,
            }),
        )),
        _ => None,
    }
}

pub(super) fn from_timer(
    kind: &'static str,
    automation_name: &str,
) -> (ExecutionContext, serde_json::Value) {
    (
        ExecutionContext::new(InvocationScope::ScheduledAutomation {
            automation_id: automation_name.to_string(),
        }),
        serde_json::json!({ "type": kind, "automation": automation_name }),
    )
}

fn sender_name(sender: &Sender) -> &'static str {
    match sender {
        Sender::User(_) => "user",
        Sender::Guest(_) => "guest",
        Sender::System => "system",
    }
}
