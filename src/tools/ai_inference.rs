use std::{borrow::Cow, time::Duration};

use async_trait::async_trait;
use reqwest::Client;
use schemars::{JsonSchema, Schema, SchemaGenerator};

use async_openai::types::chat::{CreateChatCompletionRequest, CreateChatCompletionResponse};

use crate::{
    http::HttpError,
    openai::{CreateResponse, Response},
    tools::{self, InternalTool},
};

#[derive(Clone)]
pub struct AiInferenceTool {
    client: Client,
    near_api_key: String,
    base_url: String,
}

impl AiInferenceTool {
    pub fn new(near_api_key: String) -> Result<Self, HttpError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(env_timeout_secs(
                "NEAR_CHAT_TIMEOUT_SECS",
                180,
            )))
            .build()
            .map_err(sanitize_reqwest_error)?;

        Ok(Self {
            client,
            near_api_key,
            base_url: std::env::var("NEAR_BASE_URL")
                .unwrap_or_else(|_| "https://cloud-api.near.ai".to_string()),
        })
    }

    fn chat_completions_url(&self) -> String {
        format!(
            "{}/v1/chat/completions",
            self.base_url.trim_end_matches('/')
        )
    }

    fn responses_url(&self) -> String {
        format!("{}/v1/responses", self.base_url.trim_end_matches('/'))
    }

    pub async fn execute(
        &self,
        request: CreateChatCompletionRequest,
    ) -> Result<CreateChatCompletionResponse, String> {
        let response = self
            .client
            .post(self.chat_completions_url())
            .bearer_auth(&self.near_api_key)
            .json(&request)
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

        serde_json::from_str(&body).map_err(|err| err.to_string())
    }

    pub async fn execute_response(&self, request: CreateResponse) -> Result<Response, String> {
        if let Ok(json) = serde_json::to_string_pretty(&request) {
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("llm_requests.log")
                .and_then(|mut f| {
                    use std::io::Write as _;
                    writeln!(f, "=== request ===")?;
                    writeln!(f, "{json}")?;
                    writeln!(f, "===============")
                });
        }

        let response = self
            .client
            .post(self.responses_url())
            .bearer_auth(&self.near_api_key)
            .json(&request)
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

        let response = parse_response_body(&body)?;
        if response.status != crate::openai::Status::Completed {
            return Err(format!("response status was {:?}", response.status));
        }
        Ok(response)
    }
}

fn parse_response_body(body: &str) -> Result<Response, String> {
    match serde_json::from_str(body) {
        Ok(response) => Ok(response),
        Err(original_err) => {
            let mut value = serde_json::from_str(body).map_err(|value_err| {
                format!("failed to parse response JSON: {value_err}; typed error: {original_err}")
            })?;
            normalize_near_response(&mut value);
            serde_json::from_value(value).map_err(|normalized_err| {
                format!(
                    "failed to parse response: {original_err}; after NEAR compatibility normalization: {normalized_err}"
                )
            })
        }
    }
}

fn normalize_near_response(value: &mut serde_json::Value) {
    let Some(output) = value
        .get_mut("output")
        .and_then(|output| output.as_array_mut())
    else {
        return;
    };

    for item in output {
        if item.get("type").and_then(|value| value.as_str()) != Some("reasoning") {
            continue;
        }

        if let Some(summary) = item.get_mut("summary") {
            if let Some(text) = summary.as_str() {
                *summary = if text.is_empty() {
                    serde_json::Value::Array(Vec::new())
                } else {
                    serde_json::json!([{ "type": "summary_text", "text": text }])
                };
            }
        }

        if let Some(content) = item.get_mut("content") {
            if let Some(text) = content.as_str() {
                *content = if text.is_empty() {
                    serde_json::Value::Array(Vec::new())
                } else {
                    serde_json::json!([{ "type": "reasoning_text", "text": text }])
                };
            }
        }
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(transparent)]
pub struct AiInferenceParams(pub CreateChatCompletionRequest);

impl JsonSchema for AiInferenceParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("AiInferenceParams")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        Schema::try_from(serde_json::json!({
            "type": "object",
            "description": "OpenAI-compatible CreateChatCompletionRequest.",
            "properties": {
                "model": {
                    "type": "string",
                    "description": "Optional model name. If omitted, Warden uses its configured default."
                },
                "messages": {
                    "type": "array",
                    "description": "OpenAI-compatible chat messages.",
                    "items": { "type": "object" }
                },
                "tools": {
                    "type": "array",
                    "description": "Optional OpenAI-compatible tool definitions."
                },
                "tool_choice": {
                    "description": "Optional OpenAI-compatible tool choice."
                }
            },
            "required": ["messages"],
            "additionalProperties": true
        }))
        .expect("ai_inference schema should be valid")
    }
}

#[async_trait]
impl InternalTool for AiInferenceTool {
    fn name(&self) -> &'static str {
        "ai_inference"
    }

    fn responses_schema(&self) -> crate::openai::Tool {
        tools::responses_tool_schema::<AiInferenceParams>(
            self.name(),
            "Call an OpenAI-compatible chat completion model with a chat completion request.",
        )
    }

    async fn execute(&self, args: serde_json::Value) -> Result<tools::ToolOutput, String> {
        tools::execute_typed(args, |params: AiInferenceParams| async {
            AiInferenceTool::execute(self, params.0).await
        })
        .await
        .map(tools::ToolOutput::json)
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
