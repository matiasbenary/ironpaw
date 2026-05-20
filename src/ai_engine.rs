use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    bus::{Bus, Event, Sender},
    http::HttpError,
    openai::{
        ChatCompletionResponseMessage, CreateChatCompletionRequest, CreateChatCompletionResponse,
    },
    tools::ToolsRuntime,
};

#[async_trait]
pub trait AiTools: Send + Sync + 'static {
    async fn execute_ai_inference(
        &self,
        request: CreateChatCompletionRequest,
    ) -> Result<CreateChatCompletionResponse, HttpError>;
}

#[async_trait]
impl AiTools for ToolsRuntime {
    async fn execute_ai_inference(
        &self,
        request: CreateChatCompletionRequest,
    ) -> Result<CreateChatCompletionResponse, HttpError> {
        self.ai_inference.infer(request).await
    }
}

pub struct AiEngine<T> {
    bus: Bus,
    tools: Arc<T>,
}

impl<T> AiEngine<T>
where
    T: AiTools,
{
    pub fn new(bus: Bus, tools: T) -> Self {
        Self {
            bus,
            tools: Arc::new(tools),
        }
    }

    pub fn start(&self) -> tokio::task::JoinHandle<()> {
        let bus = self.bus.clone();
        let tools = Arc::clone(&self.tools);
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
                        let request = CreateChatCompletionRequest {
                            messages: vec![message],
                            ..Default::default()
                        };
                        let message = match tools.execute_ai_inference(request).await {
                            Ok(response) => first_response_message(response),
                            Err(err) => error_message(format!("AI inference failed: {err}")),
                        };
                        bus.publish(Event::MessageOut {
                            channel,
                            recipient: Sender::User,
                            address,
                            message,
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

fn first_response_message(response: CreateChatCompletionResponse) -> ChatCompletionResponseMessage {
    response
        .choices
        .into_iter()
        .next()
        .map(|choice| choice.message)
        .unwrap_or_else(|| error_message("AI response had no choices"))
}

fn error_message(content: impl Into<String>) -> ChatCompletionResponseMessage {
    serde_json::from_value(serde_json::json!({
        "role": "assistant",
        "content": content.into(),
    }))
    .expect("static assistant error message should match OpenAI chat schema")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::timeout;

    use super::*;

    struct EchoInference;

    #[async_trait]
    impl AiTools for EchoInference {
        async fn execute_ai_inference(
            &self,
            request: CreateChatCompletionRequest,
        ) -> Result<CreateChatCompletionResponse, HttpError> {
            let prompt = serde_json::to_value(&request.messages[0])
                .unwrap()
                .get("content")
                .and_then(|content| content.as_str())
                .unwrap()
                .to_string();

            Ok(serde_json::from_value(serde_json::json!({
                "id": "test",
                "object": "chat.completion",
                "created": 0,
                "model": "test",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": format!("AI heard: {prompt}")
                    },
                    "finish_reason": "stop"
                }]
            }))
            .unwrap())
        }
    }

    #[tokio::test]
    async fn user_message_produces_message_out() {
        let bus = Bus::new(16);
        let engine = AiEngine::new(bus.clone(), EchoInference);
        let _engine_task = engine.start();
        let mut events = bus.subscribe();

        bus.publish(Event::MessageIn {
            channel: "manual".to_string(),
            sender: Sender::User,
            address: crate::bus::ChannelAddress {
                channel: "manual".to_string(),
                conversation_id: "test".to_string(),
                thread_id: None,
            },
            message: serde_json::from_value(serde_json::json!({
                "role": "user",
                "content": "hello",
            }))
            .unwrap(),
        });

        loop {
            let event = timeout(Duration::from_secs(1), events.recv())
                .await
                .expect("timed out waiting for bus event")
                .expect("bus closed");

            if let Event::MessageOut {
                channel,
                recipient,
                address,
                message,
            } = event
            {
                assert_eq!(channel, "manual");
                assert_eq!(recipient, Sender::User);
                assert_eq!(address.conversation_id, "test");
                assert_eq!(message.content.as_deref(), Some("AI heard: hello"));
                break;
            }
        }
    }
}
