use crate::{
    http::{HttpError, HttpTool},
    openai::{CreateChatCompletionRequest, CreateChatCompletionResponse},
    secrets::ToolSecrets,
};

const DEFAULT_MODEL: &str = "Qwen/Qwen3-30B-A3B-Instruct-2507";

#[derive(Clone)]
pub struct AiInferenceTool {
    http: HttpTool<ToolSecrets>,
    base_url: String,
    model: String,
}

impl AiInferenceTool {
    pub fn new(http: HttpTool<ToolSecrets>) -> Self {
        Self {
            http,
            base_url: std::env::var("NEAR_BASE_URL")
                .unwrap_or_else(|_| "https://cloud-api.near.ai".to_string()),
            model: std::env::var("NEAR_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string()),
        }
    }

    fn chat_completions_url(&self) -> String {
        format!(
            "{}/v1/chat/completions",
            self.base_url.trim_end_matches('/')
        )
    }

    pub async fn infer(
        &self,
        mut request: CreateChatCompletionRequest,
    ) -> Result<CreateChatCompletionResponse, HttpError> {
        if request.model.is_empty() {
            request.model = self.model.clone();
        }

        let body = self
            .http
            .post_json(&self.chat_completions_url(), &request)
            .await?;
        serde_json::from_str(&body).map_err(|err| HttpError::Http(err.to_string()))
    }
}
