use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use reqwest::Client;

use crate::{
    http::HttpError,
    message::{Attachment, Message, ToolCall, ToolDefinition},
    models,
    secrets::ToolSecrets,
    tools::{self, InternalTool},
};

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct RawInferenceRequest {
    #[serde(default = "models::text_model")]
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Vec<ToolDefinition>,
}

#[derive(Clone, Debug, Default, serde::Deserialize, serde::Serialize)]
pub struct RawInferenceUsage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct RawInferenceOutput {
    pub message: Message,
    pub usage: RawInferenceUsage,
}

#[derive(Clone)]
pub struct RawInferenceTool {
    client: Client,
    base_url: String,
    secrets: ToolSecrets,
}

impl RawInferenceTool {
    pub fn new(secrets: ToolSecrets) -> Result<Self, HttpError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(env_timeout_secs(
                "NEAR_CHAT_TIMEOUT_SECS",
                180,
            )))
            .build()
            .map_err(sanitize_reqwest_error)?;

        Ok(Self {
            client,
            base_url: std::env::var("NEAR_BASE_URL")
                .unwrap_or_else(|_| "https://cloud-api.near.ai".to_string()),
            secrets,
        })
    }

    fn chat_completions_url(&self) -> String {
        format!(
            "{}/v1/chat/completions",
            self.base_url.trim_end_matches('/')
        )
    }

    pub async fn execute(
        &self,
        request: RawInferenceRequest,
    ) -> Result<RawInferenceOutput, String> {
        self.execute_with_raw_response(request)
            .await
            .map(|(output, _)| output)
    }

    pub(crate) async fn execute_with_raw_response(
        &self,
        request: RawInferenceRequest,
    ) -> Result<(RawInferenceOutput, String), String> {
        let near_api_key = self
            .secrets
            .get("NEAR_API_KEY")
            .ok_or_else(|| "missing tool secret: NEAR_API_KEY".to_string())?;
        let chat_request = self.chat_request(&request)?;

        let response = self
            .client
            .post(self.chat_completions_url())
            .bearer_auth(near_api_key)
            .json(&chat_request)
            .send()
            .await
            .map_err(sanitize_reqwest_error)
            .map_err(|err| err.to_string())?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(sanitize_reqwest_error)
            .map_err(|err| err.to_string())?;

        if !status.is_success() {
            return Err(format!("request failed with {status}: {body}"));
        }

        let response = serde_json::from_str(&body)
            .map_err(|err| format!("failed to parse chat response JSON: {err}"))?;

        Ok((response_to_output(response)?, body))
    }

    fn chat_request(&self, request: &RawInferenceRequest) -> Result<serde_json::Value, String> {
        let tools = if request.tools.is_empty() {
            None
        } else {
            Some(
                request
                    .tools
                    .iter()
                    .map(tool_definition_to_chat_tool)
                    .collect::<Vec<_>>(),
            )
        };
        Ok(serde_json::json!({
            "model": request.model,
            "messages": self.messages_to_chat(&request.messages)?,
            "tools": tools,
        }))
    }

    fn messages_to_chat(&self, messages: &[Message]) -> Result<Vec<serde_json::Value>, String> {
        messages
            .iter()
            .map(|message| self.message_to_chat(message))
            .collect()
    }

    fn message_to_chat(&self, message: &Message) -> Result<serde_json::Value, String> {
        match message {
            Message::System(text) => Ok(serde_json::json!({
                "role": "system",
                "content": text,
            })),
            Message::User { text, attachments } => Ok(serde_json::json!({
                "role": "user",
                "content": self.user_content(text, attachments)?,
            })),
            Message::Assistant {
                text, tool_calls, ..
            } => {
                let tool_calls = if tool_calls.is_empty() {
                    None
                } else {
                    Some(tool_calls.iter().map(tool_call_to_chat).collect::<Vec<_>>())
                };
                Ok(serde_json::json!({
                    "role": "assistant",
                    "content": text,
                    "tool_calls": tool_calls,
                }))
            }
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

    fn user_content(
        &self,
        text: &str,
        attachments: &[Attachment],
    ) -> Result<serde_json::Value, String> {
        let mut parts = vec![serde_json::json!({
            "type": "text",
            "text": user_text_with_attachment_hints(text, attachments),
        })];
        for attachment in attachments {
            if attachment.mime_type.starts_with("image/") {
                parts.push(serde_json::json!({
                    "type": "image_url",
                    "image_url": { "url": self.attachment_data_url(attachment)? },
                }));
            }
        }
        Ok(serde_json::Value::Array(parts))
    }

    fn attachment_data_url(&self, attachment: &Attachment) -> Result<String, String> {
        let data = attachment.read().map_err(|err| err.to_string())?;
        Ok(format!(
            "data:{};base64,{}",
            attachment.mime_type,
            base64::engine::general_purpose::STANDARD.encode(data)
        ))
    }
}

fn response_to_output(response: serde_json::Value) -> Result<RawInferenceOutput, String> {
    let message = response["choices"]
        .as_array()
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .ok_or_else(|| format!("chat response missing choices[0].message: {response}"))?;
    let text = message["content"].as_str().map(str::to_string);
    let tool_calls = message["tool_calls"]
        .as_array()
        .into_iter()
        .flatten()
        .map(chat_tool_call_to_warden)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let usage = serde_json::from_value(response["usage"].clone()).unwrap_or_default();
    Ok(RawInferenceOutput {
        message: Message::Assistant {
            text,
            attachments: Vec::new(),
            tool_calls,
        },
        usage,
    })
}

fn chat_tool_call_to_warden(value: &serde_json::Value) -> Result<Option<ToolCall>, String> {
    if value["type"].as_str() != Some("function") {
        return Ok(None);
    }
    let function = value
        .get("function")
        .ok_or_else(|| format!("tool call missing function: {value}"))?;
    let arguments = function["arguments"]
        .as_str()
        .ok_or_else(|| format!("tool call arguments were not a string: {value}"))?;
    let arguments = normalize_tool_arguments(arguments);
    Ok(Some(ToolCall {
        id: value["id"]
            .as_str()
            .ok_or_else(|| format!("tool call missing id: {value}"))?
            .to_string(),
        name: function["name"]
            .as_str()
            .ok_or_else(|| format!("tool call missing function name: {value}"))?
            .to_string(),
        arguments,
    }))
}

fn normalize_tool_arguments(arguments: &str) -> String {
    // Temporary compatibility patch for DeepSeek-V4-Flash responses served by
    // NEAR/SGLang that append an empty string after otherwise valid JSON.
    if let Some(stripped) = arguments.strip_suffix("\"\"")
        && serde_json::from_str::<serde_json::Value>(arguments).is_err()
        && serde_json::from_str::<serde_json::Value>(stripped).is_ok()
    {
        return stripped.to_string();
    }
    arguments.to_string()
}

fn tool_definition_to_chat_tool(tool: &ToolDefinition) -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.parameters,
        }
    })
}

fn tool_call_to_chat(tool_call: &ToolCall) -> serde_json::Value {
    serde_json::json!({
        "id": tool_call.id,
        "type": "function",
        "function": {
            "name": tool_call.name,
            "arguments": tool_call.arguments,
        }
    })
}

fn user_text_with_attachment_hints(text: &str, attachments: &[Attachment]) -> String {
    let mut result = text.to_string();
    for attachment in attachments {
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(&format!(
            "A file is attached (file_id: '{}', mime_type: '{}'). Use the inspect_file tool to inspect it.",
            attachment.file_id, attachment.mime_type
        ));
    }
    result
}

#[async_trait]
impl InternalTool for RawInferenceTool {
    fn name(&self) -> &str {
        "raw_inference"
    }

    fn tool_definition(&self) -> crate::message::ToolDefinition {
        tools::tool_definition::<RawInferenceRequest>(
            self.name(),
            "Call a model directly with Warden-native messages: one completion, \
             no history, no tool execution. Use it to delegate a task to a \
             specific model (e.g. a vision or audio model).",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &tools::ExecutionContext,
        responder: tools::ToolResponder,
    ) -> Result<(), String> {
        let output = tools::execute_typed(args, |request: RawInferenceRequest| async {
            RawInferenceTool::execute(self, request).await
        })
        .await?;
        responder.respond(output)?;
        Ok(())
    }
}

fn sanitize_reqwest_error(err: reqwest::Error) -> HttpError {
    let kind = if err.is_timeout() {
        "timeout"
    } else if err.is_connect() {
        "connect"
    } else if err.is_request() {
        "request"
    } else if err.is_body() {
        "body"
    } else if err.is_decode() {
        "decode"
    } else {
        "request"
    };
    HttpError::Http(format!("{kind}: {}", err.without_url()))
}

fn env_timeout_secs(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::normalize_tool_arguments;

    #[test]
    fn removes_deepseek_trailing_empty_string_from_valid_json() {
        assert_eq!(normalize_tool_arguments("{}\"\""), "{}");
        assert_eq!(
            normalize_tool_arguments("{\"query\":\"x\"}\"\""),
            "{\"query\":\"x\"}"
        );
        assert_eq!(
            normalize_tool_arguments("{\"still\":\"broken\""),
            "{\"still\":\"broken\""
        );
    }
}
