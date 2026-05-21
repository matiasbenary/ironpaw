use std::sync::Arc;

use crate::{
    bus::{Bus, Event, Sender},
    openai::{
        ChatCompletionMessageToolCalls, ChatCompletionRequestMessage,
        ChatCompletionResponseMessage, ChatCompletionToolChoiceOption, CreateChatCompletionRequest,
        ToolChoiceOptions, assistant_error_message, first_response_message,
        response_message_to_request_message, tool_result_message,
    },
    tools::{InternalTool, InternalTools, send_message::SendMessageParams},
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
                        let message =
                            match run_tool_loop(internal_tools.as_ref(), vec![message]).await {
                                Ok(message) => message,
                                Err(err) => {
                                    assistant_error_message(format!("AI inference failed: {err}"))
                                }
                            };
                        let content = message.content.clone().unwrap_or_default();
                        let _ = internal_tools
                            .send_message
                            .execute(SendMessageParams {
                                channel,
                                content,
                                conversation_id: Some(address.conversation_id),
                                thread_id: address.thread_id,
                            })
                            .await;
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
    mut messages: Vec<ChatCompletionRequestMessage>,
) -> Result<ChatCompletionResponseMessage, String> {
    for _round in 0..MAX_TOOL_ROUNDS {
        let available_tools = internal_tools.tools.clone();
        let request = CreateChatCompletionRequest {
            messages: messages.clone(),
            tools: Some(
                available_tools
                    .iter()
                    .map(|tool| tool.openai_schema())
                    .collect(),
            ),
            tool_choice: Some(ChatCompletionToolChoiceOption::Mode(
                ToolChoiceOptions::Auto,
            )),
            ..Default::default()
        };

        let message = first_response_message(
            internal_tools
                .ai_inference
                .execute(request)
                .await
                .map_err(|err| err.to_string())?,
        );
        let tool_calls = message.tool_calls.clone().unwrap_or_default();
        if tool_calls.is_empty() {
            return Ok(message);
        }

        messages.push(response_message_to_request_message(&message)?);
        for tool_call in tool_calls {
            let tool_result = execute_tool_call(&available_tools, tool_call).await?;
            messages.push(tool_result);
        }
    }

    Ok(assistant_error_message("stopped after too many tool calls"))
}

async fn execute_tool_call(
    tools: &[Arc<dyn InternalTool>],
    tool_call: ChatCompletionMessageToolCalls,
) -> Result<ChatCompletionRequestMessage, String> {
    match tool_call {
        ChatCompletionMessageToolCalls::Function(function_call) => {
            let tool_call_id = function_call.id.clone();
            let Some(tool) = tools
                .iter()
                .find(|tool| tool.name() == function_call.function.name)
            else {
                return Ok(tool_result_message(
                    tool_call_id,
                    serde_json::json!({
                        "ok": false,
                        "error": format!("unknown tool: {}", function_call.function.name)
                    }),
                ));
            };
            let args = serde_json::from_str(&function_call.function.arguments)
                .map_err(|err| err.to_string())?;
            let result = tool.execute(args).await?;
            Ok(tool_result_message(tool_call_id, result))
        }
        ChatCompletionMessageToolCalls::Custom(custom_call) => Ok(tool_result_message(
            custom_call.id,
            serde_json::json!({
                "ok": false,
                "error": "custom tool calls are not supported"
            }),
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::timeout;

    use super::*;

    #[tokio::test]
    async fn tool_call_sends_message() {
        let bus = Bus::new(16);
        let mut events = bus.subscribe();
        let tools = vec![Arc::new(crate::tools::send_message::SendMessageTool::new(
            bus.clone(),
        )) as Arc<dyn InternalTool>];
        let tool_call = serde_json::from_value(serde_json::json!({
            "type": "function",
            "id": "call_1",
            "function": {
                "name": "send_message",
                "arguments": "{\"channel\":\"telegram\",\"content\":\"hi\"}"
            }
        }))
        .unwrap();

        let result = execute_tool_call(&tools, tool_call).await.unwrap();
        let result_json = serde_json::to_value(result).unwrap();
        assert_eq!(result_json["role"], "tool");
        assert_eq!(result_json["tool_call_id"], "call_1");

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
