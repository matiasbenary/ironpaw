use std::{collections::HashSet, sync::Arc};

use crate::{
    bus::{Bus, ChannelAddress, Event, Sender},
    message::{OutboundAttachment, OutboundMessage},
    openai::{CreateResponse, InputItem, InputParam},
    models,
    tools::{InternalTool, InternalTools, ToolOutput},
};

use super::{
    orchestrator::{
        ExecutionPlan, GeneratedFile, construct_execution_plan, fulfill_pending_deliveries,
        record_generated_file,
    },
    preamble::{files_shared_notice_input_item, system_preamble},
    utils::{
        fill_current_destination, parse_response_output, publish_progress, summarize_tool_args,
        tool_result_input_item,
    },
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
                                    attachments: Vec::new(),
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
    let mut input_items = system_preamble(channel, address);
    let execution_plan = construct_execution_plan(&internal_tools.ai_inference, channel, &messages)
        .await
        .unwrap_or_else(|err| {
            debug_log(format!(
                "ai_engine: execution plan construction failed: {err}"
            ));
            ExecutionPlan::default()
        });
    input_items.extend(messages);
    let mut attachments: Vec<OutboundAttachment> = Vec::new();
    let mut generated_files: Vec<GeneratedFile> = Vec::new();
    let mut fulfilled_requirements: HashSet<String> = HashSet::new();
    let mut previous_response_id: Option<String> = None;
    for round in 0..MAX_TOOL_ROUNDS {
        let available_tools: Vec<_> = internal_tools
            .tools
            .iter()
            .map(|tool| tool.responses_schema())
            .collect();
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
            previous_response_id: previous_response_id.take(),
            ..Default::default()
        };

        let response = internal_tools
            .ai_inference
            .execute(request)
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
            fulfill_pending_deliveries(
                internal_tools,
                channel,
                &execution_plan,
                &generated_files,
                &mut fulfilled_requirements,
                Some(&output.text),
            )
            .await?;
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
                    attachments,
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
            let tool_result = execute_tool_call(
                &internal_tools.tools,
                tool_call,
                Some((bus, channel, address)),
            )
            .await?;
            record_generated_file(
                &mut generated_files,
                &execution_plan,
                &tool_result.tool_name,
                &tool_result.result,
            );
            let fulfilled_items = fulfill_pending_deliveries(
                internal_tools,
                channel,
                &execution_plan,
                &generated_files,
                &mut fulfilled_requirements,
                None,
            )
            .await?;
            let new_attachments = tool_result.attachments;
            input_items.push(tool_result.input_item);
            if !new_attachments.is_empty() {
                input_items.push(files_shared_notice_input_item(
                    new_attachments.len(),
                    channel,
                ));
            }
            attachments.extend(new_attachments);
            input_items.extend(fulfilled_items);
        }
    }

    publish_current_channel_output(
        bus,
        channel,
        address,
        AgentLoopOutput {
            content: Some("stopped after too many tool calls".to_string()),
            attachments,
        },
    );
    Ok(())
}

struct AgentLoopOutput {
    content: Option<String>,
    attachments: Vec<OutboundAttachment>,
}

fn publish_current_channel_output(
    bus: &Bus,
    channel: &str,
    address: &ChannelAddress,
    output: AgentLoopOutput,
) {
    if output.content.is_none() && output.attachments.is_empty() {
        return;
    }
    bus.publish(Event::MessageOut {
        channel: channel.to_string(),
        recipient: Sender::User,
        address: Some(address.clone()),
        message: OutboundMessage {
            content: output.content,
            attachments: output.attachments,
        },
    });
}

struct ToolCallOutput {
    input_item: InputItem,
    attachments: Vec<OutboundAttachment>,
    tool_name: String,
    result: serde_json::Value,
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
    let result = tool_response.result;
    debug_log(format!("ai_engine: tool {tool_name} finished"));
    Ok(ToolCallOutput {
        input_item: tool_result_input_item(&tool_call_id, result.to_string()),
        attachments: tool_response.files,
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
        assert_eq!(result_json["call_id"], "call_1");
        assert!(
            result_json["output"]
                .as_str()
                .unwrap()
                .contains("\"ok\":true")
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
            result_json["output"]
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
                    file_id: None,
                    data_base64: Some("abc".to_string()),
                    url: None,
                }],
            })
        }
    }

    #[derive(schemars::JsonSchema)]
    struct AttachmentToolParams;
}
