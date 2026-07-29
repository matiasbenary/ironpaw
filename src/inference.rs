use std::sync::Arc;

use base64::Engine as _;
use reqwest::Client;

use crate::{
    bus::{ApiKeyError, Bus, ConversationKey, Event},
    config::ProviderConfig,
    http::HttpError,
    message::{Attachment, Message, ToolCall, ToolDefinition},
    secrets::ToolSecrets,
};

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
    client: Client,
    provider: Arc<ProviderConfig>,
    secrets: ToolSecrets,
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
        let client = Client::builder()
            .timeout(provider.chat_timeout)
            .build()
            .map_err(sanitize_reqwest_error)?;
        Ok(Self {
            bus,
            client,
            provider,
            secrets,
        })
    }

    pub fn provider(&self) -> &Arc<ProviderConfig> {
        &self.provider
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
        let near_api_key = self.secrets.get("NEAR_API_KEY").ok_or_else(|| {
            InferenceError::ApiKey("missing tool secret: NEAR_API_KEY".to_string())
        })?;
        let request = chat_request(&request).map_err(InferenceError::Other)?;
        let response = self
            .client
            .post(format!(
                "{}/v1/chat/completions",
                self.provider.base_url.trim_end_matches('/')
            ))
            .bearer_auth(near_api_key)
            .json(&request)
            .send()
            .await
            .map_err(sanitize_reqwest_error)
            .map_err(|error| InferenceError::Other(error.to_string()))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(sanitize_reqwest_error)
            .map_err(|error| InferenceError::Other(error.to_string()))?;
        if !status.is_success() {
            let message = format!("request failed with {status}: {body}");
            return Err(if is_api_key_status(status) {
                InferenceError::ApiKey(message)
            } else {
                InferenceError::Other(message)
            });
        }
        let response = serde_json::from_str(&body).map_err(|error| {
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

fn is_api_key_status(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED
            | reqwest::StatusCode::PAYMENT_REQUIRED
            | reqwest::StatusCode::FORBIDDEN
    )
}

fn sanitize_reqwest_error(error: reqwest::Error) -> HttpError {
    let kind = if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_request() {
        "request"
    } else if error.is_body() {
        "body"
    } else if error.is_decode() {
        "decode"
    } else {
        "request"
    };
    HttpError::Http(format!("{kind}: {}", error.without_url()))
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

    #[test]
    fn normalizes_deepseek_arguments_and_classifies_credential_statuses() {
        assert_eq!(normalize_tool_arguments("{}\"\""), "{}");
        assert_eq!(
            normalize_tool_arguments("{\"still\":\"broken\""),
            "{\"still\":\"broken\""
        );
        assert!(is_api_key_status(reqwest::StatusCode::UNAUTHORIZED));
        assert!(is_api_key_status(reqwest::StatusCode::PAYMENT_REQUIRED));
        assert!(is_api_key_status(reqwest::StatusCode::FORBIDDEN));
        assert!(!is_api_key_status(reqwest::StatusCode::TOO_MANY_REQUESTS));
    }
}
