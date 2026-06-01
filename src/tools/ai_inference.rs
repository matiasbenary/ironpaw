use std::{borrow::Cow, time::Duration};

use async_trait::async_trait;
use reqwest::Client;
use schemars::{JsonSchema, Schema, SchemaGenerator};

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

    fn responses_url(&self) -> String {
        format!("{}/v1/responses", self.base_url.trim_end_matches('/'))
    }

    pub async fn execute(&self, request: CreateResponse) -> Result<Response, String> {
        log_entry("request", &request);

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
            write_log_entry(&format!("error {status}"), &body);
            return Err(format!("request failed with {status}: {body}"));
        }

        let response = parse_response_body(&body)?;
        log_entry("response", &response);

        if response.status == crate::openai::Status::Failed {
            return Err(format!("response status was {:?}", response.status));
        }
        Ok(response)
    }
}

fn log_entry(label: &str, content: &impl serde::Serialize) {
    let Ok(body) = serde_json::to_string_pretty(content) else { return };
    write_log_entry(label, &body);
}

fn write_log_entry(label: &str, content: &str) {
    static INITIALIZED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    let path = "/Users/gagdiez/tmp/miniclaw/release/llm_requests.log";
    let first = INITIALIZED.set(()).is_ok();
    let header = format!("=== {label} ===");
    let sep = "=".repeat(header.len());
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(first)
        .append(!first)
        .open(path)
        .and_then(|mut f| {
            use std::io::Write as _;
            writeln!(f, "{header}")?;
            writeln!(f, "{content}")?;
            writeln!(f, "{sep}")
        });
}

fn parse_response_body(body: &str) -> Result<Response, String> {
    match serde_json::from_str(body) {
        Ok(response) => Ok(response),
        Err(original_err) => {
            let value = serde_json::from_str(body).map_err(|value_err| {
                format!("failed to parse response JSON: {value_err}; typed error: {original_err}")
            })?;
            serde_json::from_value(value).map_err(|normalized_err| {
                format!(
                    "failed to parse response: {original_err}; after NEAR compatibility normalization: {normalized_err}"
                )
            })
        }
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(transparent)]
pub struct AiInferenceParams(pub serde_json::Value);

impl JsonSchema for AiInferenceParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("AiInferenceParams")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        Schema::try_from(serde_json::json!({
            "type": "object",
            "description": "OpenAI-compatible CreateResponse request body (responses API).",
            "properties": {
                "model": {
                    "type": "string",
                    "description": "Optional model name. If omitted, Warden uses its configured default."
                },
                "input": {
                    "type": "array",
                    "description": "Responses API input items.",
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
            "required": ["input"],
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
            let request: CreateResponse = serde_json::from_value(params.0)
                .map_err(|err| err.to_string())?;
            AiInferenceTool::execute(self, request).await
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
