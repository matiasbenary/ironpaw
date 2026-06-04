use std::sync::Arc;

use crate::{
    bus::{Bus, ChannelAddress, Event, Sender},
    message::OutboundMessage,
    models,
    openai::{
        CreateResponse, EasyInputContent, EasyInputMessage, InputItem, InputParam, Role,
        parse_response_output,
    },
    tools::{self, InternalTool, InternalTools, ToolOutput},
};

use super::{
    preamble::system_preamble,
    utils::{fill_current_destination, publish_progress, summarize_tool_args},
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
    messages: Vec<InputItem>,
) -> Result<(), String> {
    internal_tools
        .history
        .append(&address.conversation_id, &messages);

    let mut pending_tool_results: Vec<InputItem> = Vec::new();

    for round in 0..MAX_TOOL_ROUNDS {
        let available_tools: Vec<_> = internal_tools
            .tools
            .iter()
            .map(|tool| tools::schema_for_model(tool.as_ref()))
            .collect();
        let input_items = model_input_items(
            internal_tools,
            channel,
            address,
            pending_tool_results.clone(),
        );
        debug_log(format!(
            "ai_engine: round {} calling responses(input_items={}, tools={})",
            round + 1,
            input_items.len(),
            available_tools.len()
        ));
        let request = CreateResponse {
            model: Some(models::text_model()),
            input: InputParam::Items(input_items.clone()),
            tools: Some(available_tools),
            parallel_tool_calls: Some(false),
            ..Default::default()
        };

        let response = internal_tools
            .ai_inference
            .execute(request)
            .await
            .map_err(|err| err.to_string())?;
        let output = parse_response_output(response);
        let function_calls = output.function_calls;
        pending_tool_results = Vec::new();

        if function_calls.is_empty() {
            debug_log(format!(
                "ai_engine: round {} finished with assistant text",
                round + 1
            ));
            if !output.text.is_empty() {
                internal_tools.history.append(
                    &address.conversation_id,
                    &[assistant_history_input_item(output.text.clone())],
                );
            }
            publish_current_channel_output(
                bus,
                channel,
                address,
                AgentLoopOutput {
                    content: if output.text.is_empty() {
                        None
                    } else {
                        Some(output.text)
                    },
                },
            );
            return Ok(());
        }

        debug_log(format!(
            "ai_engine: round {} received {} tool call(s)",
            round + 1,
            function_calls.len()
        ));
        for tool_call in function_calls {
            let tool_name = tool_call.name.clone();
            let args = tool_call.arguments.clone();
            internal_tools.history.append(
                &address.conversation_id,
                &[assistant_history_input_item(format!(
                    "[tool use] calling {tool_name} with arguments: {args}"
                ))],
            );
            let tool_result = execute_tool_call(
                &internal_tools.tools,
                tool_call,
                Some((bus, channel, address)),
            )
            .await?;
            internal_tools.history.append(
                &address.conversation_id,
                &[assistant_history_input_item(format!(
                    "[tool use] {tool_name} {}",
                    tool_result.status
                ))],
            );
            pending_tool_results.push(tool_result.input_item);
        }
    }

    internal_tools.history.append(
        &address.conversation_id,
        &[assistant_history_input_item(
            "[tool use] stopped after too many tool calls".to_string(),
        )],
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

fn model_input_items(
    internal_tools: &InternalTools,
    channel: &str,
    address: &ChannelAddress,
    pending_tool_results: Vec<InputItem>,
) -> Vec<InputItem> {
    let mut input_items = system_preamble(channel, address);
    input_items.extend(internal_tools.history.load(&address.conversation_id));
    input_items.extend(pending_tool_results);
    input_items
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
        message: OutboundMessage {
            content: output.content,
            attachments: Vec::new(),
        },
    });
}

struct ToolCallOutput {
    input_item: InputItem,
    status: String,
}

async fn execute_tool_call(
    tools: &[Arc<dyn InternalTool>],
    tool_call: crate::openai::FunctionToolCall,
    progress: Option<(&Bus, &str, &ChannelAddress)>,
) -> Result<ToolCallOutput, String> {
    let tool_name = tool_call.name.clone();
    let args_summary = summarize_tool_args(&tool_call.arguments);
    let progress_message = format!("calling {tool_name}({args_summary})");
    debug_log(format!("ai_engine: {progress_message}"));
    if let Some((bus, channel, address)) = progress {
        publish_progress(bus, channel, address, progress_message);
    }
    let mut args: serde_json::Value =
        serde_json::from_str(&tool_call.arguments).map_err(|err| err.to_string())?;
    let (tool_response, status) = if let Some(tool) = tools.iter().find(|t| t.name() == tool_name) {
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
            Ok(response) => (response, "succeeded".to_string()),
            Err(err) => {
                debug_log(format!("ai_engine: tool {tool_name} failed: {err}"));
                (
                    ToolOutput::json(serde_json::json!({
                        "ok": false,
                        "error": err,
                    })),
                    format!("failed: {err}"),
                )
            }
        }
    } else {
        (
            ToolOutput::json(serde_json::json!({
                "ok": false,
                "error": format!("unknown tool: {tool_name}"),
            })),
            "failed: unknown tool".to_string(),
        )
    };
    let result = tool_response.result;
    debug_log(format!("ai_engine: tool {tool_name} finished"));
    Ok(ToolCallOutput {
        input_item: user_history_input_item(format!(
            "[tool response] {tool_name} returned: {}",
            result
        )),
        status,
    })
}

fn assistant_history_input_item(text: String) -> InputItem {
    EasyInputMessage {
        r#type: Default::default(),
        role: Role::Assistant,
        content: EasyInputContent::Text(text),
        phase: None,
    }
    .into()
}

fn user_history_input_item(text: String) -> InputItem {
    EasyInputMessage {
        r#type: Default::default(),
        role: Role::User,
        content: EasyInputContent::Text(text),
        phase: None,
    }
    .into()
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
                .contains("[tool response] send_message returned")
        );
        assert!(
            result_json["content"]
                .as_str()
                .unwrap()
                .contains("\"ok\":true")
        );
        assert_eq!(result.status, "succeeded");

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
}
