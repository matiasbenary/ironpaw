use std::sync::Arc;

use crate::{
    bus::{Bus, Event, InvocationScope},
    history::{History, HistoryEntry, HistoryScope, MessageMetadata},
    message::{Message, ToolCall},
    tools::{ExecutionContext, ToolError, ToolRegistry, run_registered_tool_for_context},
};

pub struct ToolHandler {
    bus: Bus,
    registry: Arc<ToolRegistry>,
    history: Arc<dyn History>,
}

impl ToolHandler {
    pub fn new(bus: Bus, registry: Arc<ToolRegistry>, history: Arc<dyn History>) -> Self {
        Self {
            bus,
            registry,
            history,
        }
    }

    pub fn start(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = Arc::clone(self);
        let mut events = self.bus.subscribe();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(Event::ToolRequest {
                        context,
                        tool_call,
                        args,
                        record_in_history,
                    }) => this.spawn_tool_request(context, tool_call, args, record_in_history),
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }

    fn spawn_tool_request(
        &self,
        context: ExecutionContext,
        tool_call: ToolCall,
        args: serde_json::Value,
        record_in_history: bool,
    ) {
        let bus = self.bus.clone();
        let registry = Arc::clone(&self.registry);
        let history = Arc::clone(&self.history);
        if record_in_history && let Err(err) = self.persist_tool_call(&context, &tool_call) {
            bus.publish(Event::ToolResult {
                context,
                tool_call_id: tool_call.id,
                tool_name: tool_call.name,
                result: Err(ToolError::new(err)),
            });
            return;
        }
        tokio::spawn(async move {
            run_registered_tool_for_context(
                bus,
                Arc::clone(&history),
                &registry,
                &context,
                tool_call.id,
                tool_call.name,
                args,
                record_in_history,
            )
            .await;
        });
    }

    fn persist_tool_call(
        &self,
        ctx: &ExecutionContext,
        tool_call: &ToolCall,
    ) -> Result<(), String> {
        let scope = history_scope(ctx);
        self.history.append_scope(
            &scope,
            &[HistoryEntry {
                message: Message::Assistant {
                    text: None,
                    attachments: Vec::new(),
                    tool_calls: vec![tool_call.clone()],
                },
                metadata: history_metadata(&scope),
            }],
        )
    }
}

pub(crate) fn persist_tool_result(
    history: &dyn History,
    ctx: &ExecutionContext,
    message: &Message,
) -> Result<(), String> {
    let scope = history_scope(ctx);
    let Message::ToolResult { .. } = message else {
        return Ok(());
    };
    history.append_scope(
        &scope,
        &[HistoryEntry {
            message: bounded_history_message(message),
            metadata: history_metadata(&scope),
        }],
    )
}

fn bounded_history_message(message: &Message) -> Message {
    const MAX_STRING_BYTES: usize = 8_000;
    const MAX_ARRAY_ITEMS: usize = 100;
    let Message::ToolResult {
        tool_call_id,
        tool_name,
        result,
    } = message
    else {
        return message.clone();
    };
    Message::ToolResult {
        tool_call_id: tool_call_id.clone(),
        tool_name: tool_name.clone(),
        result: bound_json(result, MAX_STRING_BYTES, MAX_ARRAY_ITEMS),
    }
}

fn bound_json(
    value: &serde_json::Value,
    max_string_bytes: usize,
    max_array_items: usize,
) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) => {
            if text.len() <= max_string_bytes {
                return value.clone();
            }
            let mut end = max_string_bytes;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            serde_json::Value::String(format!("{}\n[truncated in durable history]", &text[..end]))
        }
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .take(max_array_items)
                .map(|item| bound_json(item, max_string_bytes, max_array_items))
                .collect(),
        ),
        serde_json::Value::Object(fields) => serde_json::Value::Object(
            fields
                .iter()
                .map(|(key, value)| {
                    (
                        key.clone(),
                        bound_json(value, max_string_bytes, max_array_items),
                    )
                })
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn history_scope(ctx: &ExecutionContext) -> HistoryScope {
    match &ctx.invoked_by {
        InvocationScope::Conversation(conversation) => {
            HistoryScope::Conversation(conversation.clone())
        }
        InvocationScope::ScheduledAutomation { automation_id } => {
            HistoryScope::Automation(automation_id.clone())
        }
    }
}

fn history_metadata(scope: &HistoryScope) -> MessageMetadata {
    match scope {
        HistoryScope::Conversation(conversation) => {
            MessageMetadata::new(conversation.clone(), None)
        }
        HistoryScope::Automation(automation_id) => {
            MessageMetadata::for_automation(automation_id.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_large_tool_result_strings_for_history() {
        let message = Message::ToolResult {
            tool_call_id: "call-1".to_string(),
            tool_name: "web_fetch".to_string(),
            result: serde_json::json!({
                "file_id": 7,
                "body": "x".repeat(9_000),
            }),
        };
        let Message::ToolResult { result, .. } = bounded_history_message(&message) else {
            panic!("expected tool result");
        };
        assert_eq!(result["file_id"], 7);
        assert!(
            result["body"]
                .as_str()
                .unwrap()
                .contains("truncated in durable history")
        );
    }
}
