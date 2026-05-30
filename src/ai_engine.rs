use std::sync::Arc;

use crate::{
    bus::{Bus, ChannelAddress, Event, Sender},
    message::{OutboundAttachment, OutboundMessage},
    openai::{
        CreateResponse, EasyInputContent, EasyInputMessage, FunctionCallOutput,
        FunctionCallOutputItemParam, InputItem, InputParam, Item, OutputItem, OutputMessageContent,
        Role, Tool,
    },
    tools::{InternalTool, InternalTools, ToolOutput},
};

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
                        let tool_loop = match run_tool_loop(
                            internal_tools.as_ref(),
                            &bus,
                            &channel,
                            &address,
                            vec![message],
                        )
                        .await
                        {
                            Ok(output) => output,
                            Err(err) => ToolLoopOutput {
                                content: Some(format!("AI inference failed: {err}")),
                                attachments: Vec::new(),
                            },
                        };
                        let tool_loop =
                            if tool_loop.content.is_none() && tool_loop.attachments.is_empty() {
                                ToolLoopOutput {
                                    content: Some(
                                        "AI inference failed: empty assistant response".to_string(),
                                    ),
                                    attachments: Vec::new(),
                                }
                            } else {
                                tool_loop
                            };
                        bus.publish(Event::MessageOut {
                            channel,
                            recipient: Sender::User,
                            address: Some(address),
                            message: OutboundMessage {
                                content: tool_loop.content,
                                attachments: tool_loop.attachments,
                            },
                        });
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }
}

async fn run_tool_loop(
    internal_tools: &InternalTools,
    bus: &Bus,
    channel: &str,
    address: &ChannelAddress,
    messages: Vec<InputItem>,
) -> Result<ToolLoopOutput, String> {
    let mut input_items = system_preamble(channel, address);
    input_items.extend(messages);
    let mut last_tool_call: Option<String> = None;
    let mut repeated_tool_call_count = 0;
    let mut attachments: Vec<OutboundAttachment> = Vec::new();
    let mut previous_response_id: Option<String> = None;
    for round in 0..MAX_TOOL_ROUNDS {
        let available_tools = responses_tools(&internal_tools.tools);
        debug_log(format!(
            "ai_engine: round {} calling responses(input_items={}, tools={})",
            round + 1,
            input_items.len(),
            available_tools.len()
        ));
        let request = CreateResponse {
            input: InputParam::Items(input_items.clone()),
            tools: Some(available_tools),
            parallel_tool_calls: Some(false),
            previous_response_id: previous_response_id.take(),
            ..Default::default()
        };

        let response = internal_tools
            .ai_inference
            .execute_response(request)
            .await
            .map_err(|err| err.to_string())?;
        previous_response_id = Some(response.id.clone());
        let output = parse_response_output(response);
        let function_calls = output.function_calls;
        // On subsequent rounds we use previous_response_id, so only keep the new tool results.
        input_items = Vec::new();

        if function_calls.is_empty() {
            debug_log(format!(
                "ai_engine: round {} finished with assistant text",
                round + 1
            ));
            return Ok(ToolLoopOutput {
                content: if output.text.is_empty() {
                    None
                } else {
                    Some(output.text)
                },
                attachments,
            });
        }

        debug_log(format!(
            "ai_engine: round {} received {} tool call(s)",
            round + 1,
            function_calls.len()
        ));
        for tool_call in function_calls {
            let tool_call_signature = tool_call_signature(&tool_call);
            if last_tool_call.as_deref() == Some(tool_call_signature.as_str()) {
                repeated_tool_call_count += 1;
                if repeated_tool_call_count >= 2 {
                    return Ok(ToolLoopOutput {
                        content: Some(format!(
                            "stopped after repeated identical tool call: {tool_call_signature}"
                        )),
                        attachments,
                    });
                }
            } else {
                last_tool_call = Some(tool_call_signature);
                repeated_tool_call_count = 0;
            }
            let tool_result = execute_tool_call(
                &internal_tools.tools,
                tool_call,
                Some((bus, channel, address)),
            )
            .await?;
            let new_attachments = tool_result.attachments;
            input_items.push(tool_result.input_item);
            if !new_attachments.is_empty() {
                input_items.push(files_shared_notice_input_item(new_attachments.len(), channel));
            }
            attachments.extend(new_attachments);
        }
    }

    Ok(ToolLoopOutput {
        content: Some("stopped after too many tool calls".to_string()),
        attachments,
    })
}

struct ToolLoopOutput {
    content: Option<String>,
    attachments: Vec<OutboundAttachment>,
}

struct ToolCallOutput {
    input_item: InputItem,
    attachments: Vec<OutboundAttachment>,
}

async fn execute_tool_call(
    tools: &[Arc<dyn InternalTool>],
    tool_call: crate::openai::FunctionToolCall,
    progress: Option<(&Bus, &str, &ChannelAddress)>,
) -> Result<ToolCallOutput, String> {
    let tool_call_id = tool_call.call_id.clone();
    let tool_name = tool_call.name.clone();
    let args_summary = summarize_tool_args(&tool_call.arguments);
    let progress_message = format!("calling {tool_name}({args_summary})");
    debug_log(format!("ai_engine: {progress_message}"));
    if let Some((bus, channel, address)) = progress {
        publish_progress(bus, channel, address, progress_message);
    }
    let tool_response = if let Some(tool) = tools.iter().find(|tool| tool.name() == tool_name) {
        let mut args = serde_json::from_str(&tool_call.arguments).map_err(|err| err.to_string())?;
        if tool_name == "send_message" {
            fill_current_destination(&mut args, progress);
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
    debug_log(format!("ai_engine: tool {tool_name} finished"));
    Ok(ToolCallOutput {
        input_item: tool_result_input_item(
            &tool_name,
            &tool_call_id,
            tool_response.result.to_string(),
        ),
        attachments: tool_response.files,
    })
}

fn debug_log(message: impl AsRef<str>) {
    eprintln!("{}", message.as_ref());
}

fn tool_call_signature(tool_call: &crate::openai::FunctionToolCall) -> String {
    format!("{}({})", tool_call.name, tool_call.arguments)
}

struct ParsedResponseOutput {
    function_calls: Vec<crate::openai::FunctionToolCall>,
    text: String,
}

fn parse_response_output(response: crate::openai::Response) -> ParsedResponseOutput {
    let mut function_calls = Vec::new();
    let mut text_parts = Vec::new();

    for item in response.output {
        match &item {
            OutputItem::Message(message) => {
                for content in &message.content {
                    match content {
                        OutputMessageContent::OutputText(text) => {
                            if !text.text.trim().is_empty() {
                                text_parts.push(text.text.clone());
                            }
                        }
                        OutputMessageContent::Refusal(refusal) => {
                            text_parts.push(refusal.refusal.clone());
                        }
                    }
                }
            }
            OutputItem::FunctionCall(call) => {
                function_calls.push(call.clone());
            }
            _ => {}
        }
    }

    ParsedResponseOutput {
        function_calls,
        text: text_parts.join("\n\n"),
    }
}

fn responses_tools(tools: &[Arc<dyn InternalTool>]) -> Vec<Tool> {
    tools.iter().map(|tool| tool.responses_schema()).collect()
}

fn fill_current_destination(
    args: &mut serde_json::Value,
    progress: Option<(&Bus, &str, &ChannelAddress)>,
) {
    let Some((_bus, channel, address)) = progress else {
        return;
    };
    let Some(object) = args.as_object_mut() else {
        return;
    };
    if object.get("channel").is_some() {
        return;
    }
    object.insert(
        "channel".to_string(),
        serde_json::Value::String(channel.to_string()),
    );
    object
        .entry("conversation_id")
        .or_insert_with(|| serde_json::Value::String(address.conversation_id.clone()));
    if let Some(thread_id) = &address.thread_id {
        object
            .entry("thread_id")
            .or_insert_with(|| serde_json::Value::String(thread_id.clone()));
    }
}

fn system_preamble(channel: &str, address: &ChannelAddress) -> Vec<InputItem> {
    let mut text = format!(
        "You are a personal assistant. \
        The user is communicating with you through the '{channel}' channel \
        (conversation_id: '{}'",
        address.conversation_id,
    );
    if let Some(thread_id) = &address.thread_id {
        text.push_str(&format!(", thread_id: '{thread_id}'"));
    }
    text.push(')');
    text.push('.');

    vec![EasyInputMessage {
        r#type: Default::default(),
        role: Role::System,
        content: EasyInputContent::Text(text),
        phase: None,
    }
    .into()]
}

fn files_shared_notice_input_item(count: usize, channel: &str) -> InputItem {
    EasyInputMessage {
        r#type: Default::default(),
        role: Role::System,
        content: EasyInputContent::Text(format!(
            "{count} file(s) automatically shared with the user on channel '{channel}'."
        )),
        phase: None,
    }
    .into()
}

fn tool_result_input_item(
    _tool_name: &str,
    tool_call_id: &str,
    result: impl AsRef<str>,
) -> InputItem {
    Item::from(FunctionCallOutputItemParam {
        call_id: tool_call_id.to_string(),
        output: FunctionCallOutput::Text(result.as_ref().to_string()),
        id: None,
        status: None,
    })
    .into()
}

fn publish_progress(bus: &Bus, channel: &str, address: &ChannelAddress, content: String) {
    bus.publish(Event::MessageOut {
        channel: channel.to_string(),
        recipient: Sender::User,
        address: Some(address.clone()),
        message: OutboundMessage {
            content: Some(content),
            attachments: Vec::new(),
        },
    });
}

fn summarize_tool_args(arguments: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return truncate(arguments, 600);
    };
    truncate(&value.to_string(), 600)
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
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
            std::path::PathBuf::from("/tmp/warden-test-images"),
        )) as Arc<dyn InternalTool>];
        let tool_call = crate::openai::FunctionToolCall {
            call_id: "call_1".to_string(),
            namespace: None,
            name: "send_message".to_string(),
            arguments: "{\"channel\":\"telegram\",\"content\":\"hi\"}".to_string(),
            id: None,
            status: None,
        };

        let result = execute_tool_call(&tools, tool_call, None).await.unwrap();
        let result_json = serde_json::to_value(result.input_item).unwrap();
        assert_eq!(result_json["role"], "user");
        assert!(
            result_json["content"]
                .as_str()
                .unwrap()
                .contains("[Tool `send_message` call `call_1` returned:")
        );

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
        assert_eq!(message.content.as_deref(), Some("hi"));
    }

    #[tokio::test]
    async fn tool_call_carries_attachments() {
        let tools = vec![Arc::new(AttachmentTool) as Arc<dyn InternalTool>];
        let tool_call = crate::openai::FunctionToolCall {
            call_id: "call_1".to_string(),
            namespace: None,
            name: "make_attachment".to_string(),
            arguments: "{}".to_string(),
            id: None,
            status: None,
        };

        let result = execute_tool_call(&tools, tool_call, None).await.unwrap();
        assert_eq!(result.attachments.len(), 1);
        assert_eq!(result.attachments[0].filename, "generated.png");
        let result_json = serde_json::to_value(result.input_item).unwrap();
        assert!(
            result_json["content"]
                .as_str()
                .unwrap()
                .contains("\"status\":\"ok\"")
        );
    }

    struct AttachmentTool;

    #[async_trait::async_trait]
    impl InternalTool for AttachmentTool {
        fn name(&self) -> &'static str {
            "make_attachment"
        }

        fn responses_schema(&self) -> crate::openai::Tool {
            crate::tools::responses_tool_schema::<AttachmentToolParams>(
                self.name(),
                "Make a test attachment.",
            )
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
        ) -> Result<crate::tools::ToolOutput, String> {
            Ok(crate::tools::ToolOutput {
                result: serde_json::json!({ "status": "ok" }),
                files: vec![OutboundAttachment {
                    kind: "image".to_string(),
                    mime_type: "image/png".to_string(),
                    filename: "generated.png".to_string(),
                    data_base64: Some("abc".to_string()),
                    url: None,
                }],
            })
        }
    }

    #[derive(schemars::JsonSchema)]
    struct AttachmentToolParams;
}
