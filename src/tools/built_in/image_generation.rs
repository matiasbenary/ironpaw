use std::{
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;

use crate::{
    models,
    secrets::ToolSecrets,
    tools::{self, InternalTool},
};

pub struct ImageGenerationTool {
    client: reqwest::Client,
    base_url: String,
    model: String,
    files_dir: PathBuf,
    secrets: ToolSecrets,
}

impl ImageGenerationTool {
    pub fn new(files_dir: PathBuf, secrets: ToolSecrets) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .expect("image generation HTTP client"),
            base_url: std::env::var("NEAR_BASE_URL")
                .unwrap_or_else(|_| "https://cloud-api.near.ai".to_string()),
            model: models::image_model(),
            files_dir,
            secrets,
        }
    }

    async fn generate(&self, prompt: &str) -> Result<String, String> {
        let near_api_key = self
            .secrets
            .get("NEAR_API_KEY")
            .ok_or_else(|| "missing tool secret: NEAR_API_KEY".to_string())?;
        let url = format!(
            "{}/v1/images/generations",
            self.base_url.trim_end_matches('/')
        );
        let body = serde_json::json!({
            "model": self.model,
            "prompt": prompt,
            "response_format": "b64_json",
            "n": 1,
        });
        let response = self
            .client
            .post(&url)
            .bearer_auth(near_api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.without_url().to_string())?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| e.without_url().to_string())?;
        if !status.is_success() {
            return Err(format!("image generation failed with {status}: {text}"));
        }
        let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        value["data"][0]["b64_json"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| format!("image generation response missing b64_json: {text}"))
    }
}

#[derive(schemars::JsonSchema, serde::Deserialize)]
pub struct ImageGenerationParams {
    #[schemars(description = "A detailed description of the image to generate.")]
    pub prompt: String,
}

#[async_trait]
impl InternalTool for ImageGenerationTool {
    fn name(&self) -> &str {
        "image_generation"
    }

    fn tool_definition(&self) -> crate::message::ToolDefinition {
        tools::tool_definition::<ImageGenerationParams>(
            self.name(),
            "Generate an image from a text description and return a file_id. To deliver the image, call send_message with that file_id.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &crate::tools::ExecutionContext,
        responder: crate::tools::ToolResponder,
    ) -> Result<(), String> {
        let params: ImageGenerationParams =
            serde_json::from_value(args).map_err(|err| err.to_string())?;
        let data_base64 = self.generate(&params.prompt).await?;
        let file_id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;
        std::fs::create_dir_all(&self.files_dir).map_err(|e| e.to_string())?;
        let path = self.files_dir.join(format!("{file_id}.png"));
        let bytes = base64_decode(&data_base64)?;
        std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
        responder.respond(serde_json::json!({
            "status": "ok",
            "file_id": file_id,
        }))?;
        Ok(())
    }
}

fn base64_decode(data: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| e.to_string())
}
