use std::sync::Arc;

use crate::{
    bus::{Bus, ChannelAddress, Event, Sender},
    message::{Message, ToolCall},
    models,
    tools::{self, InternalTool, InternalTools, ToolOutput},
};

use super::{
    preamble::system_preamble,
    utils::{fill_current_destination, publish_progress, summarize_tool_args},
};
use crate::tools::ai_inference::AiInferenceRequest;

const MAX_TOOL_ROUNDS: usize = 8;

pub struct AiEngine {
    bus: Bus,
    internal_tools: Arc<InternalTools>,
}

impl AiEngine {
    pub fn new(bus: Bus, internal_tools: InternalTools) -> Self {
        Self {
            bus,
            internal_tools: Arc::new(internal_tools),
        }
    }

    pub fn start(&self) -> tokio::task::JoinHandle<()> {
        let bus = self.bus.clone();
        let internal_tools = Arc::clone(&self.internal_tools);
        let mut events = bus.subscribe();

        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(Event::MessageIn {
                        channel,
                        sender: Sender::User,
                        address,
                        message,
                    }) => {
                        debug_log(format!("ai_engine: received user message from {channel}"));
                        if let Err(err) = run_agent_loop(
                            internal_tools.as_ref(),
                            &bus,
                            &channel,
                            &address,
                            vec![message],
                        )
                        .await
                        {
                            publish_current_channel_output(
                                &bus,
                                &channel,
                                &address,
                                AgentLoopOutput {
                                    content: Some(format!("AI inference failed: {err}")),
                                },
                            );
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }
}

async fn run_agent_loop(
    internal_tools: &InternalTools,
    bus: &Bus,
    channel: &str,
    address: &ChannelAddress,
    messages: Vec<Message>,
) -> Result<(), String> {
    internal_tools
        .history
        .append(&address.conversation_id, &messages);

    let mut transient_messages = Vec::new();

    for round in 0..MAX_TOOL_ROUNDS {
        let available_tools: Vec<_> = internal_tools
            .tools
            .iter()
            .map(|tool| tools::definition_for_model(tool.as_ref()))
            .collect();
        let model_messages =
            model_messages(internal_tools, channel, address, transient_messages.clone());
        debug_log(format!(
            "ai_engine: round {} calling chat_completions(messages={}, tools={})",
            round + 1,
            model_messages.len(),
            available_tools.len()
        ));
        let request = AiInferenceRequest {
            model: models::text_model(),
            messages: model_messages,
            tools: available_tools,
            parallel_tool_calls: false,
        };

        let output = internal_tools
            .ai_inference
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
                "ai_engine: round {} finished with assistant text",
                round + 1
            ));
            if text.as_deref().is_some_and(|text| !text.is_empty()) {
                internal_tools
                    .history
                    .append(&address.conversation_id, &[output.clone()]);
            }
            publish_current_channel_output(
                bus,
                channel,
                address,
                AgentLoopOutput {
                    content: text.filter(|text| !text.is_empty()),
                },
            );
            return Ok(());
        }

        debug_log(format!(
            "ai_engine: round {} received {} tool call(s)",
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
            let tool_result = execute_tool_call(
                &internal_tools.tools,
                tool_call,
                Some((bus, channel, address)),
            )
            .await?;
            tool_results.push(tool_result);
        }
        if has_malformed_tool_call {
            transient_messages.push(assistant_message_for_history(output));
            transient_messages.extend(tool_results);
        } else {
            internal_tools
                .history
                .append(&address.conversation_id, &[output]);
            internal_tools
                .history
                .append(&address.conversation_id, &tool_results);
        }
    }

    internal_tools.history.append(
        &address.conversation_id,
        &[Message::Assistant {
            text: Some("[tool use] stopped after too many tool calls".to_string()),
            attachments: Vec::new(),
            tool_calls: Vec::new(),
        }],
    );
    publish_current_channel_output(
        bus,
        channel,
        address,
        AgentLoopOutput {
            content: Some("stopped after too many tool calls".to_string()),
        },
    );
    Ok(())
}

fn model_messages(
    internal_tools: &InternalTools,
    channel: &str,
    address: &ChannelAddress,
    transient_messages: Vec<Message>,
) -> Vec<Message> {
    let mut messages = system_preamble(channel, address);
    messages.extend(internal_tools.history.load(&address.conversation_id));
    messages.extend(transient_messages);
    messages
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

struct AgentLoopOutput {
    content: Option<String>,
}

fn publish_current_channel_output(
    bus: &Bus,
    channel: &str,
    address: &ChannelAddress,
    output: AgentLoopOutput,
) {
    if output.content.is_none() {
        return;
    }
    bus.publish(Event::MessageOut {
        channel: channel.to_string(),
        recipient: Sender::User,
        address: Some(address.clone()),
        message: Message::Assistant {
            text: output.content,
            attachments: Vec::new(),
            tool_calls: Vec::new(),
        },
    });
}

async fn execute_tool_call(
    tools: &[Arc<dyn InternalTool>],
    tool_call: ToolCall,
    progress: Option<(&Bus, &str, &ChannelAddress)>,
) -> Result<Message, String> {
    let tool_name = tool_call.name.clone();
    let args_summary = summarize_tool_args(&tool_call.arguments);
    let progress_message = format!("calling {tool_name}({args_summary})");
    debug_log(format!("ai_engine: {progress_message}"));
    if let Some((bus, channel, address)) = progress {
        publish_progress(bus, channel, address, progress_message);
    }
    let mut args: serde_json::Value = match serde_json::from_str(&tool_call.arguments) {
        Ok(args) => args,
        Err(err) => {
            return Ok(Message::ToolResult {
                tool_call_id: tool_call.id,
                tool_name,
                result: serde_json::json!({
                    "ok": false,
                    "error": err.to_string(),
                }),
            });
        }
    };
    let tool_response = if let Some(tool) = tools.iter().find(|t| t.name() == tool_name) {
        if tool_name == "send_message" {
            fill_current_destination(&mut args, progress);
        }
        if tool_name == "todo" {
            if let Some((_bus, _channel, address)) = progress {
                if let Some(obj) = args.as_object_mut() {
                    obj.insert(
                        "conversation_id".to_string(),
                        serde_json::Value::String(address.conversation_id.clone()),
                    );
                }
            }
        }
        match tool.execute(args).await {
            Ok(response) => response,
            Err(err) => {
                debug_log(format!("ai_engine: tool {tool_name} failed: {err}"));
                ToolOutput::json(serde_json::json!({
                    "ok": false,
                    "error": err,
                }))
            }
        }
    } else {
        ToolOutput::json(serde_json::json!({
            "ok": false,
            "error": format!("unknown tool: {tool_name}"),
        }))
    };
    let result = tool_response.result;
    debug_log(format!("ai_engine: tool {tool_name} finished"));
    Ok(Message::ToolResult {
        tool_call_id: tool_call.id,
        tool_name,
        result,
    })
}

fn debug_log(message: impl AsRef<str>) {
    eprintln!("{}", message.as_ref());
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::timeout;

    use super::*;

    #[test]
    fn fill_current_destination_leaves_explicit_channel_defaults_to_channel() {
        let bus = Bus::new(16);
        let address = ChannelAddress {
            channel: "tui".to_string(),
            conversation_id: "http://127.0.0.1:60684".to_string(),
            thread_id: None,
        };
        let mut args = serde_json::json!({
            "channel": "telegram",
            "content": "hi"
        });

        fill_current_destination(&mut args, Some((&bus, "tui", &address)));

        assert_eq!(args["channel"], "telegram");
        assert!(args.get("conversation_id").is_none());
        assert!(args.get("thread_id").is_none());
    }

    #[test]
    fn fill_current_destination_fills_current_conversation_when_channel_omitted() {
        let bus = Bus::new(16);
        let address = ChannelAddress {
            channel: "telegram".to_string(),
            conversation_id: "123".to_string(),
            thread_id: Some("456".to_string()),
        };
        let mut args = serde_json::json!({
            "content": "hi"
        });

        fill_current_destination(&mut args, Some((&bus, "telegram", &address)));

        assert_eq!(args["channel"], "telegram");
        assert_eq!(args["conversation_id"], "123");
        assert_eq!(args["thread_id"], "456");
    }

    #[tokio::test]
    async fn tool_call_sends_message() {
        let bus = Bus::new(16);
        let mut events = bus.subscribe();
        let tools = vec![Arc::new(crate::tools::send_message::SendMessageTool::new(
            bus.clone(),
            |_| Err("unexpected attachment lookup".to_string()),
        )) as Arc<dyn InternalTool>];
        let tool_call = ToolCall {
            id: "call_1".to_string(),
            name: "send_message".to_string(),
            arguments: serde_json::json!({
                "channel": "telegram",
                "content": "hi",
            })
            .to_string(),
        };

        let result = execute_tool_call(&tools, tool_call, None).await.unwrap();
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
        let tools = vec![];
        let tool_call = ToolCall {
            id: "call_bad".to_string(),
            name: "send_message".to_string(),
            arguments: "{\"channel\":\"telegram\"".to_string(),
        };

        let result = execute_tool_call(&tools, tool_call, None).await.unwrap();
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
