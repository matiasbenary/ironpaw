use std::{
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;

use crate::{
    message::OutboundAttachment,
    openai::FunctionCallOutput,
    tools::{self, InternalTool, ToolOutput},
};

const DEFAULT_IMAGE_MODEL: &str = "black-forest-labs/FLUX.2-klein-4B";

pub struct ImageGenerationTool {
    client: reqwest::Client,
    near_api_key: String,
    base_url: String,
    model: String,
    files_dir: PathBuf,
}

impl ImageGenerationTool {
    pub fn new(near_api_key: String, files_dir: PathBuf) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .expect("image generation HTTP client"),
            near_api_key,
            base_url: std::env::var("NEAR_BASE_URL")
                .unwrap_or_else(|_| "https://cloud-api.near.ai".to_string()),
            model: std::env::var("NEAR_IMAGE_MODEL")
                .unwrap_or_else(|_| DEFAULT_IMAGE_MODEL.to_string()),
            files_dir,
        }
    }

    async fn generate(&self, prompt: &str) -> Result<String, String> {
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
            .bearer_auth(&self.near_api_key)
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
    fn name(&self) -> &'static str {
        "image_generation"
    }

    fn responses_schema(&self) -> crate::openai::Tool {
        tools::responses_tool_schema::<ImageGenerationParams>(
            self.name(),
            "Generate an image from a text description. The image is automatically delivered to the current channel. To also send it to a different channel, call send_message with the returned file_id.",
        )
    }

    async fn execute(&self, _args: serde_json::Value) -> Result<ToolOutput, String> {
        return mock_image(&self.files_dir); // DELETE THIS LINE TO USE REAL IMAGE GENERATION
        #[allow(unreachable_code)]
        let params: ImageGenerationParams =
            serde_json::from_value(_args).map_err(|err| err.to_string())?;
        let data_base64 = self.generate(&params.prompt).await?;
        let file_id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;
        std::fs::create_dir_all(&self.files_dir).map_err(|e| e.to_string())?;
        let path = self.files_dir.join(format!("{file_id}.png"));
        let bytes = base64_decode(&data_base64)?;
        std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
        Ok(ToolOutput {
            model_output: FunctionCallOutput::Text(serde_json::json!({
                "status": "ok",
                "file_id": file_id,
            }).to_string()),
            files: vec![OutboundAttachment {
                kind: "image".to_string(),
                mime_type: "image/png".to_string(),
                filename: format!("{file_id}.png"),
                file_id: Some(file_id),
                data_base64: Some(data_base64),
                url: None,
            }],
        })
    }
}

fn base64_decode(data: &str) -> Result<Vec<u8>, String> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| e.to_string())
}

fn mock_image(files_dir: &PathBuf) -> Result<ToolOutput, String> {
    use base64::Engine as _;
    let src = files_dir.join("1780086002.png");
    let bytes = std::fs::read(&src).map_err(|e| format!("mock image read error: {e}"))?;
    let data_base64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(ToolOutput {
        model_output: FunctionCallOutput::Text(serde_json::json!({ "status": "stored", "file_id": 1780086002u32 }).to_string()),
        files: vec![OutboundAttachment {
            kind: "image".to_string(),
            mime_type: "image/png".to_string(),
            filename: "1780086002.png".to_string(),
            file_id: Some(1780086002u32),
            data_base64: Some(data_base64),
            url: None,
        }],
    })
}
