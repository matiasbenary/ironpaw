use std::sync::Arc;

use crate::{
    bus::{ApiKeyError, Bus, ConversationKey, Event},
    config::ProviderConfig,
    http::{HttpError, NetworkGate, NetworkGrant},
    message::{Attachment, Message, ToolCall, ToolDefinition},
    secrets::ToolSecrets,
};
use base64::Engine as _;

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct InferenceRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
}

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct InferenceUsage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct InferenceOutput {
    pub message: Message,
    pub usage: InferenceUsage,
}

#[derive(serde::Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
    usage: Option<InferenceUsage>,
}

#[derive(serde::Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

#[derive(serde::Deserialize)]
struct ChatMessage {
    content: Option<String>,
    tool_calls: Option<Vec<ChatToolCall>>,
}

#[derive(serde::Deserialize)]
#[serde(tag = "type")]
enum ChatToolCall {
    #[serde(rename = "function")]
    Function { id: String, function: ChatFunction },
    #[serde(other)]
    Other,
}

#[derive(serde::Deserialize)]
struct ChatFunction {
    name: String,
    arguments: String,
}

#[derive(Debug)]
pub enum InferenceError {
    ApiKey(String),
    Other(String),
}

impl std::fmt::Display for InferenceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApiKey(message) | Self::Other(message) => message.fmt(formatter),
        }
    }
}

/// Executes model requests and reports conversation-scoped credential errors.
#[derive(Clone)]
pub struct Inference {
    bus: Bus,
    http: NetworkGate<ToolSecrets>,
    provider: Arc<ProviderConfig>,
}

impl Inference {
    pub fn new(bus: Bus, secrets: ToolSecrets) -> Result<Self, HttpError> {
        Self::with_config(bus, secrets, Arc::new(ProviderConfig::from_env()))
    }

    pub fn with_config(
        bus: Bus,
        secrets: ToolSecrets,
        provider: Arc<ProviderConfig>,
    ) -> Result<Self, HttpError> {
        let host = reqwest::Url::parse(&provider.base_url)
            .map_err(|error| HttpError::InvalidRequest(error.to_string()))?
            .host_str()
            .ok_or_else(|| HttpError::InvalidRequest("provider URL has no host".to_string()))?
            .to_string();
        Ok(Self {
            bus,
            http: NetworkGate::new(secrets, vec![NetworkGrant::bearer(host, "NEAR_API_KEY")])
                .with_timeout(provider.chat_timeout),
            provider,
        })
    }

    pub fn provider(&self) -> &Arc<ProviderConfig> {
        &self.provider
    }

    pub(crate) async fn post_json<T>(
        &self,
        path: &str,
        body: &T,
    ) -> Result<crate::http::HttpResponse, HttpError>
    where
        T: serde::Serialize + ?Sized,
    {
        self.http
            .post_json(
                &format!("{}{}", self.provider.base_url.trim_end_matches('/'), path),
                body,
            )
            .await
    }

    pub async fn execute(
        &self,
        request: InferenceRequest,
        conversation: Option<&ConversationKey>,
    ) -> Result<InferenceOutput, InferenceError> {
        let result = self.near_inference(request).await;
        if let Err(error) = &result
            && let Some(conversation) = conversation
            && matches!(error, InferenceError::ApiKey(_))
        {
            self.bus.publish(Event::ApiKeyError {
                conversation: conversation.clone(),
                error: ApiKeyError {
                    message: error.to_string(),
                },
            });
        }
        result
    }

    async fn near_inference(
        &self,
        request: InferenceRequest,
    ) -> Result<InferenceOutput, InferenceError> {
        let request = chat_request(&request).map_err(InferenceError::Other)?;
        let response = self
            .post_json("/v1/chat/completions", &request)
            .await
            .map_err(|error| match error {
                HttpError::MissingCredential(_) => InferenceError::ApiKey(error.to_string()),
                _ => InferenceError::Other(error.to_string()),
            })?;
        if !(200..300).contains(&response.status) {
            let status = response.status;
            let body = response.body;
            let message = format!("request failed with {status}: {body}");
            return Err(if is_api_key_status(status) {
                InferenceError::ApiKey(message)
            } else {
                InferenceError::Other(message)
            });
        }
        let response = serde_json::from_str(&response.body).map_err(|error| {
            InferenceError::Other(format!("failed to parse chat response JSON: {error}"))
        })?;
        response_to_output(response).map_err(InferenceError::Other)
    }
}

fn response_to_output(response: ChatResponse) -> Result<InferenceOutput, String> {
    let message = response
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| "chat response missing choices[0].message".to_string())?
        .message;
    let tool_calls = message
        .tool_calls
        .into_iter()
        .flatten()
        .filter_map(|call| match call {
            ChatToolCall::Function { id, function } => Some(ToolCall {
                id,
                name: function.name,
                arguments: normalize_tool_arguments(&function.arguments),
            }),
            ChatToolCall::Other => None,
        })
        .collect();
    Ok(InferenceOutput {
        message: Message::Assistant {
            text: message.content.map(|text| strip_thinking(&text)),
            attachments: Vec::new(),
            tool_calls,
        },
        usage: response.usage.unwrap_or_default(),
    })
}

fn normalize_tool_arguments(arguments: &str) -> String {
    if let Some(stripped) = arguments.strip_suffix("\"\"")
        && serde_json::from_str::<serde_json::Value>(arguments).is_err()
        && serde_json::from_str::<serde_json::Value>(stripped).is_ok()
    {
        return stripped.to_string();
    }
    arguments.to_string()
}

fn strip_thinking(text: &str) -> String {
    text.rsplit_once("</think>").map_or_else(
        || text.to_string(),
        |(_, answer)| answer.trim_start().to_string(),
    )
}

fn tool_definition_to_chat_tool(tool: &ToolDefinition) -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": { "name": tool.name, "description": tool.description, "parameters": tool.parameters }
    })
}

fn tool_call_to_chat(tool_call: &ToolCall) -> serde_json::Value {
    serde_json::json!({
        "id": tool_call.id,
        "type": "function",
        "function": { "name": tool_call.name, "arguments": tool_call.arguments }
    })
}

fn chat_request(request: &InferenceRequest) -> Result<serde_json::Value, String> {
    let tools = (!request.tools.is_empty()).then(|| {
        request
            .tools
            .iter()
            .map(tool_definition_to_chat_tool)
            .collect::<Vec<_>>()
    });
    Ok(serde_json::json!({
        "model": request.model,
        "messages": request.messages.iter().map(message_to_chat).collect::<Result<Vec<_>, _>>()?,
        "tools": tools,
    }))
}

fn message_to_chat(message: &Message) -> Result<serde_json::Value, String> {
    match message {
        Message::System(text) => Ok(serde_json::json!({ "role": "system", "content": text })),
        Message::User { text, attachments } => Ok(serde_json::json!({
            "role": "user",
            "content": user_content(text, attachments)?,
        })),
        Message::Assistant {
            text, tool_calls, ..
        } => Ok(serde_json::json!({
            "role": "assistant",
            "content": text,
            "tool_calls": (!tool_calls.is_empty())
                .then(|| tool_calls.iter().map(tool_call_to_chat).collect::<Vec<_>>()),
        })),
        Message::ToolResult {
            tool_call_id,
            result,
            ..
        } => Ok(serde_json::json!({
            "role": "tool",
            "tool_call_id": tool_call_id,
            "content": result.to_string(),
        })),
    }
}

fn user_content(text: &str, attachments: &[Attachment]) -> Result<serde_json::Value, String> {
    let mut parts = vec![serde_json::json!({ "type": "text", "text": text })];
    for attachment in attachments {
        if attachment.mime_type.starts_with("image/") {
            parts.push(serde_json::json!({
                "type": "image_url",
                "image_url": { "url": attachment_data_url(attachment)? },
            }));
        }
    }
    Ok(serde_json::Value::Array(parts))
}

fn attachment_data_url(attachment: &Attachment) -> Result<String, String> {
    let data = attachment.read().map_err(|error| error.to_string())?;
    Ok(format!(
        "data:{};base64,{}",
        attachment.mime_type,
        base64::engine::general_purpose::STANDARD.encode(data)
    ))
}

fn is_api_key_status(status: u16) -> bool {
    matches!(status, 401 | 402 | 403)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::Secrets;

    #[tokio::test]
    async fn missing_key_is_reported_to_the_conversation() {
        let bus = Bus::new(1);
        let mut events = bus.subscribe();
        let service = Inference::new(bus, Secrets::for_tests().tools.clone()).expect("inference");
        let conversation = ConversationKey {
            channel: "local-client".to_string(),
            chat_id: "test".to_string(),
            thread_id: None,
        };
        let error = service
            .execute(
                InferenceRequest {
                    model: "test".to_string(),
                    messages: vec![Message::User {
                        text: "hello".to_string(),
                        attachments: Vec::new(),
                    }],
                    tools: Vec::new(),
                },
                Some(&conversation),
            )
            .await
            .expect_err("missing key must fail");
        assert!(error.to_string().contains("NEAR_API_KEY"));
        let Event::ApiKeyError {
            conversation: reported,
            ..
        } = events.recv().await.expect("API-key event")
        else {
            panic!("expected API-key event");
        };
        assert_eq!(reported, conversation);
    }
}
