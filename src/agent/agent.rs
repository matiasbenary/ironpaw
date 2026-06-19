use std::sync::Arc;

use crate::{
    bus::{Bus, ChannelAction, ChannelAddress},
    history::{HistoryEntry, MessageMetadata},
    message::{Message, ToolCall},
    models,
    tools::{self, InternalTools, ToolContext},
};

use super::{
    preamble::system_preamble,
    utils::{publish_channel_action, publish_progress, summarize_tool_args},
};
use crate::tools::raw_inference::RawInferenceRequest;

const MAX_TOOL_ROUNDS: usize = 8;

/// The destination a run replies to: the channel and address the triggering
/// event came from. `None` for background runs (e.g. `call_agent` tasks).
pub type Destination<'a> = Option<(&'a str, &'a ChannelAddress)>;

/// The bounded tool loop: history + raw_inference + tool execution. Who sees
/// the final text is the caller's decision: an automation publishes it to a
/// channel, a parent agent consumes it as a tool result.
pub struct Agent {
    bus: Bus,
    internal_tools: Arc<InternalTools>,
}

impl Agent {
    pub fn new(bus: Bus, internal_tools: Arc<InternalTools>) -> Self {
        Self {
            bus,
            internal_tools,
        }
    }

    /// Run the tool loop on a conversation and return the final assistant
    /// text (None when the model finished without text).
    pub async fn run(
        &self,
        destination: Destination<'_>,
        conversation_id: &str,
        messages: Vec<Message>,
    ) -> Result<Option<String>, String> {
        let ctx = ToolContext {
            destination: destination
                .map(|(channel, address)| (channel.to_string(), address.clone())),
            conversation_id: Some(conversation_id.to_string()),
            ..ToolContext::default()
        };
        let mut transient_messages = if self.messages_are_in_history(conversation_id, &messages) {
            Vec::new()
        } else {
            messages
        };

        for round in 0..MAX_TOOL_ROUNDS {
            if let Some((channel, address)) = destination {
                publish_channel_action(&self.bus, channel, address, ChannelAction::Typing);
            }
            let available_tools: Vec<_> = self
                .internal_tools
                .all()
                .iter()
                .map(|tool| tools::definition_for_model(tool.as_ref()))
                .collect();
            let model_messages =
                self.model_messages(destination, conversation_id, transient_messages.clone());
            debug_log(format!(
                "agent: round {} calling chat_completions(messages={}, tools={})",
                round + 1,
                model_messages.len(),
                available_tools.len()
            ));
            let request = RawInferenceRequest {
                model: models::text_model(),
                messages: model_messages,
                tools: available_tools,
                parallel_tool_calls: false,
            };

            let output = self
                .internal_tools
                .raw_inference
                .execute(request)
                .await
                .map_err(|err| err.to_string())?;
            let Message::Assistant {
                text, tool_calls, ..
            } = output.clone()
            else {
                return Err("model did not return an assistant message".to_string());
            };
            transient_messages.clear();
            if tool_calls.is_empty() {
                debug_log(format!(
                    "agent: round {} finished with assistant text",
                    round + 1
                ));
                if text.as_deref().is_some_and(|text| !text.is_empty()) {
                    self.internal_tools.history.append(
                        conversation_id,
                        &[history_entry(
                            output.clone(),
                            destination,
                            conversation_id,
                            false,
                        )],
                    );
                }
                return Ok(text.filter(|text| !text.is_empty()));
            }

            debug_log(format!(
                "agent: round {} received {} tool call(s)",
                round + 1,
                tool_calls.len()
            ));
            let mut tool_results = Vec::new();
            let mut has_malformed_tool_call = false;
            for tool_call in &tool_calls {
                if serde_json::from_str::<serde_json::Value>(&tool_call.arguments).is_err() {
                    has_malformed_tool_call = true;
                }
            }
            for tool_call in tool_calls {
                self.publish_tool_progress(destination, &tool_call.name, &tool_call.arguments);
                let tool_result = execute_tool_call(&self.internal_tools, tool_call, &ctx).await;
                tool_results.push(tool_result);
            }
            if has_malformed_tool_call {
                transient_messages.push(assistant_message_for_history(output));
                transient_messages.extend(tool_results);
            } else {
                self.internal_tools.history.append(
                    conversation_id,
                    &[history_entry(output, destination, conversation_id, false)],
                );
                self.internal_tools.history.append(
                    conversation_id,
                    &history_entries(tool_results, destination, conversation_id, false),
                );
            }
        }

        let stopped = "stopped after too many tool calls".to_string();
        self.internal_tools.history.append(
            conversation_id,
            &[history_entry(
                Message::Assistant {
                    text: Some(format!("[tool use] {stopped}")),
                    attachments: Vec::new(),
                    tool_calls: Vec::new(),
                },
                destination,
                conversation_id,
                false,
            )],
        );
        Ok(Some(stopped))
    }

    fn model_messages(
        &self,
        destination: Destination<'_>,
        conversation_id: &str,
        transient_messages: Vec<Message>,
    ) -> Vec<Message> {
        let mut messages = system_preamble(destination);
        messages.extend(
            self.internal_tools
                .history
                .load(conversation_id)
                .into_iter()
                .map(|entry| entry.message),
        );
        messages.extend(transient_messages);
        messages
    }

    fn messages_are_in_history(&self, conversation_id: &str, messages: &[Message]) -> bool {
        if messages.is_empty() {
            return true;
        }
        let history = self.internal_tools.history.load(conversation_id);
        if history.len() < messages.len() {
            return false;
        }
        history[history.len() - messages.len()..]
            .iter()
            .map(|entry| &entry.message)
            .eq(messages.iter())
    }

    fn publish_tool_progress(
        &self,
        destination: Destination<'_>,
        tool_name: &str,
        arguments: &str,
    ) {
        let args_summary = summarize_tool_args(arguments);
        let progress_message = format!("calling {tool_name}({args_summary})");
        debug_log(format!("agent: {progress_message}"));
        if let Some((channel, address)) = destination {
            publish_progress(&self.bus, channel, address, progress_message);
        }
    }
}

fn history_entries(
    messages: Vec<Message>,
    destination: Destination<'_>,
    conversation_id: &str,
    include_external_id: bool,
) -> Vec<HistoryEntry> {
    messages
        .into_iter()
        .map(|message| history_entry(message, destination, conversation_id, include_external_id))
        .collect()
}

fn history_entry(
    message: Message,
    destination: Destination<'_>,
    conversation_id: &str,
    include_external_id: bool,
) -> HistoryEntry {
    HistoryEntry {
        message,
        metadata: history_metadata(destination, conversation_id, include_external_id),
    }
}

fn history_metadata(
    destination: Destination<'_>,
    conversation_id: &str,
    include_external_id: bool,
) -> MessageMetadata {
    if let Some((channel, address)) = destination {
        return MessageMetadata::new(
            channel,
            address.conversation_id.clone(),
            address.thread_id.clone(),
            include_external_id
                .then(|| address.message_id.clone())
                .flatten(),
        );
    }
    MessageMetadata::new("warden", conversation_id.to_string(), None, None)
}

async fn execute_tool_call(
    tools: &InternalTools,
    tool_call: ToolCall,
    ctx: &ToolContext,
) -> Message {
    let tool_name = tool_call.name;
    let args: serde_json::Value = match serde_json::from_str(&tool_call.arguments) {
        Ok(args) => args,
        Err(err) => {
            return Message::ToolResult {
                tool_call_id: tool_call.id,
                tool_name,
                result: error_result(err.to_string()),
            };
        }
    };
    let result = tools::run_tool(tools, &tool_name, args, ctx).await;
    debug_log(format!("agent: tool {tool_name} finished"));
    Message::ToolResult {
        tool_call_id: tool_call.id,
        tool_name,
        result,
    }
}

fn error_result(error: impl Into<String>) -> serde_json::Value {
    serde_json::json!({ "ok": false, "error": error.into() })
}

fn assistant_message_for_history(message: Message) -> Message {
    let Message::Assistant {
        text,
        attachments,
        tool_calls,
    } = message
    else {
        return message;
    };
    Message::Assistant {
        text,
        attachments,
        tool_calls: tool_calls
            .into_iter()
            .map(|mut tool_call| {
                if serde_json::from_str::<serde_json::Value>(&tool_call.arguments).is_err() {
                    tool_call.arguments = "{}".to_string();
                }
                tool_call
            })
            .collect(),
    }
}

fn debug_log(message: impl AsRef<str>) {
    eprintln!("{}", message.as_ref());
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::timeout;

    use super::*;
    use crate::bus::Event;

    fn test_tools(bus: &Bus) -> InternalTools {
        let files_dir = std::env::temp_dir().join("warden-agent-test-files");
        std::fs::create_dir_all(&files_dir).unwrap();
        let history = std::env::temp_dir().join(format!(
            "warden-agent-test-history-{}.db",
            uuid::Uuid::new_v4()
        ));
        InternalTools::new(
            crate::tools::raw_inference::RawInferenceTool::new().unwrap(),
            crate::tools::send_message::SendMessageTool::new(bus.clone(), |_| {
                Err("unexpected attachment lookup".to_string())
            }),
            crate::tools::delete_message::DeleteMessageTool::new(bus.clone()),
            files_dir,
            Arc::new(crate::history::SqliteHistory::new(history).unwrap()),
        )
    }

    #[tokio::test]
    async fn tool_call_sends_message() {
        let bus = Bus::new(16);
        let mut events = bus.subscribe();
        let tools = test_tools(&bus);
        let tool_call = ToolCall {
            id: "call_1".to_string(),
            name: "send_message".to_string(),
            arguments: serde_json::json!({
                "channel": "telegram",
                "content": "hi",
            })
            .to_string(),
        };

        let result = execute_tool_call(&tools, tool_call, &ToolContext::default()).await;
        let Message::ToolResult {
            tool_call_id,
            tool_name,
            result: tool_result,
        } = result
        else {
            panic!("expected tool result");
        };
        assert_eq!(tool_call_id, "call_1");
        assert_eq!(tool_name, "send_message");
        assert_eq!(tool_result["ok"], true);

        let event = timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("timed out waiting for bus event")
            .expect("bus closed");

        let Event::MessageOut {
            channel,
            address,
            message,
            ..
        } = event
        else {
            panic!("expected MessageOut");
        };
        assert_eq!(channel, "telegram");
        assert!(address.is_none());
        let Message::Assistant { text, .. } = message else {
            panic!("expected assistant message");
        };
        assert_eq!(text.as_deref(), Some("hi"));
    }

    #[tokio::test]
    async fn malformed_tool_arguments_return_tool_result_error() {
        let bus = Bus::new(16);
        let tools = test_tools(&bus);
        let tool_call = ToolCall {
            id: "call_bad".to_string(),
            name: "send_message".to_string(),
            arguments: "{\"channel\":\"telegram\"".to_string(),
        };

        let result = execute_tool_call(&tools, tool_call, &ToolContext::default()).await;
        let Message::ToolResult {
            tool_call_id,
            tool_name,
            result: tool_result,
        } = result
        else {
            panic!("expected tool result");
        };
        assert_eq!(tool_call_id, "call_bad");
        assert_eq!(tool_name, "send_message");
        assert_eq!(tool_result["ok"], false);
        assert!(
            tool_result["error"]
                .as_str()
                .unwrap()
                .contains("EOF while parsing")
        );
    }

    #[test]
    fn malformed_tool_arguments_are_sanitized_for_history() {
        let message = Message::Assistant {
            text: None,
            attachments: Vec::new(),
            tool_calls: vec![ToolCall {
                id: "call_bad".to_string(),
                name: "send_message".to_string(),
                arguments: "{\"channel\":\"telegram\"".to_string(),
            }],
        };

        let Message::Assistant { tool_calls, .. } = assistant_message_for_history(message) else {
            panic!("expected assistant message");
        };
        assert_eq!(tool_calls[0].arguments, "{}");
    }
}
