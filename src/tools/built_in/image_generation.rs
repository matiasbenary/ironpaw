use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;

use crate::{
    config::ProviderConfig,
    message::{available_filename, execution_files_dir},
    secrets::ToolSecrets,
    tools::{self, InternalTool, ToolError},
};

pub struct ImageGenerationTool {
    client: reqwest::Client,
    base_url: String,
    model: String,
    files_dir: PathBuf,
    secrets: ToolSecrets,
}

impl ImageGenerationTool {
    pub fn new(files_dir: PathBuf, secrets: ToolSecrets, provider: Arc<ProviderConfig>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(provider.image_timeout)
                .build()
                .expect("image generation HTTP client"),
            base_url: provider.base_url.clone(),
            model: provider.image_model.clone(),
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
            .map_err(|e| format!("request: {}", e.without_url()))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| format!("request: {}", e.without_url()))?;
        if !status.is_success() {
            return Err(format!("request failed with {status}: {text}"));
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
    #[schemars(
        description = "A short descriptive filename for the generated image. The .png extension is enforced."
    )]
    pub filename: String,
}

#[async_trait]
impl InternalTool for ImageGenerationTool {
    fn name(&self) -> &str {
        "image_generation"
    }

    fn tool_definition(&self) -> crate::message::ToolDefinition {
        tools::tool_definition::<ImageGenerationParams>(
            self.name(),
            "Generate an image from a text description, store it as a PNG, and return its filename. To deliver the image, call send_message with that filename.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &crate::tools::ExecutionContext,
        responder: crate::tools::ToolResponder,
    ) {
        let result = async {
            let params: ImageGenerationParams =
                serde_json::from_value(args).map_err(|err| ToolError::new(err.to_string()))?;
            let data_base64 = self
                .generate(&params.prompt)
                .await
                .map_err(ToolError::new)?;
            let directory = execution_files_dir(&self.files_dir, ctx);
            std::fs::create_dir_all(&directory).map_err(|e| ToolError::new(e.to_string()))?;
            let filename =
                available_filename(&directory, Some(&params.filename), "generated-image", "png");
            let path = directory.join(&filename);
            let bytes = base64_decode(&data_base64).map_err(ToolError::new)?;
            std::fs::write(&path, bytes).map_err(|e| ToolError::new(e.to_string()))?;
            Ok(serde_json::json!({
                "filename": filename,
            }))
        }
        .await;
        responder.respond(result);
    }
}

fn base64_decode(data: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| e.to_string())
}
