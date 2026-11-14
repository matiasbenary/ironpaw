use std::sync::Arc;

use crate::{
    config::ProviderConfig,
    message::{Attachment, Message, ToolCall, ToolDefinition},
    network::http::{HttpError, NetworkGate, NetworkGrant},
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

/// Executes model requests.
#[derive(Clone)]
pub struct Inference {
    http: NetworkGate<ToolSecrets>,
    provider: Arc<ProviderConfig>,
}

impl Inference {
    pub fn new(secrets: ToolSecrets) -> Result<Self, HttpError> {
        Self::with_config(secrets, Arc::new(ProviderConfig::from_env()))
    }

    pub fn with_config(
        secrets: ToolSecrets,
        provider: Arc<ProviderConfig>,
    ) -> Result<Self, HttpError> {
        let host = reqwest::Url::parse(&provider.base_url)
            .map_err(|error| HttpError::InvalidRequest(error.to_string()))?
            .host_str()
            .ok_or_else(|| HttpError::InvalidRequest("provider URL has no host".to_string()))?
            .to_string();
        Ok(Self {
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
    ) -> Result<crate::network::http::HttpResponse, HttpError>
    where
        T: serde::Serialize + ?Sized,
    {
        let url = format!("{}{}", self.provider.base_url.trim_end_matches('/'), path);
        let mut attempt = 0;
        loop {
            match self.http.post_json(&url, body).await {
                Err(error) if is_connection_failure(&error) && attempt < CONNECTION_RETRIES => {
                    attempt += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(500 * attempt)).await;
                }
                result => return result,
            }
        }
    }

    /// Transcribes speech through an OpenAI-compatible audio endpoint: the local whisper
    /// server when `WHISPER_URL` is set, otherwise the provider's.
    pub async fn transcribe(
        &self,
        filename: String,
        mime_type: String,
        data_base64: String,
    ) -> Result<String, String> {
        let http = self.http.clone();
        let fields = serde_json::json!({ "model": self.provider.transcription_model }).to_string();
        let whisper = self.provider.whisper_url.clone();
        let url = format!(
            "{}/v1/audio/transcriptions",
            whisper
                .as_deref()
                .unwrap_or(&self.provider.base_url)
                .trim_end_matches('/')
        );
        let timeout = self.provider.chat_timeout;
        let body = tokio::task::spawn_blocking(move || {
            if whisper.is_none() {
                return http
                    .post_multipart_base64_blocking(
                        &url,
                        &fields,
                        "file",
                        &filename,
                        &mime_type,
                        &data_base64,
                    )
                    .map_err(|error| error.to_string());
            }
            // Local, no credentials: outside the network gate (loopback http needs local-development).
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&data_base64)
                .map_err(|error| error.to_string())?;
            let part = reqwest::blocking::multipart::Part::bytes(bytes)
                .file_name(filename)
                .mime_str(&mime_type)
                .map_err(|error| error.to_string())?;
            crate::network::client::blocking_for_https_or_loopback(&url, timeout)?
                .post(&url)
                .multipart(reqwest::blocking::multipart::Form::new().part("file", part))
                .send()
                .and_then(|response| response.error_for_status())
                .and_then(|response| response.text())
                .map_err(|error| error.without_url().to_string())
        })
        .await
        .map_err(|error| error.to_string())??;
        let value: serde_json::Value =
            serde_json::from_str(&body).map_err(|error| error.to_string())?;
        value["text"]
            .as_str()
            .map(|text| text.trim().to_string())
            .ok_or_else(|| format!("transcription response missing text: {body}"))
    }

    pub async fn execute(
        &self,
        request: InferenceRequest,
    ) -> Result<InferenceOutput, InferenceError> {
        self.near_inference(request).await
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

/// Extra tries after a request that never got an answer (connection reset, EOF, refused).
const CONNECTION_RETRIES: u64 = 2;

/// True for failures to reach the provider. Timeouts are not retried (they already waited the
/// full chat timeout) and neither are HTTP error statuses, which carry a real answer.
fn is_connection_failure(error: &HttpError) -> bool {
    matches!(error, HttpError::Http(message)
        if message.contains("error sending request") && !message.contains("timed out"))
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
    async fn missing_key_is_reported_as_an_error() {
        let service = Inference::new(Secrets::for_tests().tools.clone()).expect("inference");
        let error = service
            .execute(InferenceRequest {
                model: "test".to_string(),
                messages: vec![Message::User {
                    text: "hello".to_string(),
                    attachments: Vec::new(),
                }],
                tools: Vec::new(),
            })
            .await
            .expect_err("missing key must fail");
        assert!(error.to_string().contains("NEAR_API_KEY"));
    }

    #[test]
    fn only_connection_failures_are_retried() {
        let http = |message: &str| HttpError::Http(message.to_string());
        assert!(is_connection_failure(&http(
            "error sending request: client error (Connect): Connection reset by peer (os error 104)"
        )));
        assert!(is_connection_failure(&http(
            "error sending request: client error (Connect): unexpected EOF"
        )));
        assert!(!is_connection_failure(&http(
            "error sending request: operation timed out (timed out)"
        )));
        assert!(!is_connection_failure(&http("request failed with 500")));
        assert!(!is_connection_failure(&HttpError::MissingCredential(
            "NEAR_API_KEY".to_string()
        )));
    }
}
