use reqwest::Client;

use crate::{
    inference::{Inference, InferenceRequest},
    message::Message,
    secrets::ToolSecrets,
};

use super::sqlite::MemoryRecord;

const EXTRACTION_PROMPT: &str = include_str!("extraction_prompt.txt");
#[derive(serde::Deserialize)]
pub(super) struct ExtractedMemory {
    pub text: String,
    #[serde(default = "user_attribution")]
    pub attributed_to: String,
}

#[derive(serde::Deserialize)]
struct ExtractionResponse {
    #[serde(default)]
    memory: Vec<ExtractedMemory>,
}

pub(super) struct MemoryInference {
    client: Client,
    secrets: ToolSecrets,
    inference: Inference,
}

impl MemoryInference {
    pub fn new(secrets: ToolSecrets, inference: Inference) -> Result<Self, String> {
        let client = Client::builder()
            .timeout(inference.provider().chat_timeout)
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self {
            client,
            secrets,
            inference,
        })
    }

    pub async fn extract(
        &self,
        previous_messages: &[Message],
        new_messages: &[Message],
        existing: &[MemoryRecord],
    ) -> Result<Vec<ExtractedMemory>, String> {
        let existing = existing
            .iter()
            .enumerate()
            .map(|(id, memory)| serde_json::json!({ "id": id, "text": memory.text }))
            .collect::<Vec<_>>();
        let today = chrono::Utc::now().date_naive().to_string();
        let input = serde_json::json!({
            "previous_messages": previous_messages,
            "existing_memories": existing,
            "new_messages": new_messages,
            "observation_date": today,
            "current_date": today,
        });
        let response = self
            .inference
            .execute(
                InferenceRequest {
                    model: self.inference.provider().text_model.clone(),
                    messages: vec![
                        Message::System(EXTRACTION_PROMPT.to_string()),
                        Message::User {
                            text: input.to_string(),
                            attachments: Vec::new(),
                        },
                    ],
                    tools: Vec::new(),
                },
                None,
            )
            .await
            .map_err(|error| error.to_string())?;
        let Message::Assistant {
            text: Some(text), ..
        } = response.message
        else {
            return Err("memory extraction returned no text".to_string());
        };
        serde_json::from_str::<ExtractionResponse>(json_object(&text)?)
            .map(|response| response.memory)
            .map_err(|error| format!("invalid memory extraction: {error}"))
    }

    pub async fn embed(&self, text: &str) -> Result<Vec<f32>, String> {
        let mut embeddings = self.embed_batch(&[text]).await?;
        embeddings
            .pop()
            .ok_or_else(|| "NEAR returned no embedding".to_string())
    }

    pub async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let api_key = self
            .secrets
            .get("NEAR_API_KEY")
            .ok_or_else(|| "missing tool secret: NEAR_API_KEY".to_string())?;
        let response = self
            .client
            .post(format!(
                "{}/v1/embeddings",
                self.inference.provider().base_url.trim_end_matches('/')
            ))
            .bearer_auth(api_key)
            .json(&serde_json::json!({
                "model": &self.inference.provider().embedding_model,
                "input": texts,
                "encoding_format": "float",
            }))
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        let body = response.text().await.map_err(|error| error.to_string())?;
        if !status.is_success() {
            return Err(format!("NEAR embeddings failed with {status}: {body}"));
        }
        let mut response: EmbeddingResponse =
            serde_json::from_str(&body).map_err(|error| error.to_string())?;
        response.data.sort_by_key(|item| item.index);
        if response.data.len() != texts.len() {
            return Err(format!(
                "NEAR returned {} embeddings for {} inputs",
                response.data.len(),
                texts.len()
            ));
        }
        Ok(response
            .data
            .into_iter()
            .map(|item| item.embedding)
            .collect())
    }
}

#[derive(serde::Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingItem>,
}

#[derive(serde::Deserialize)]
struct EmbeddingItem {
    index: usize,
    embedding: Vec<f32>,
}

fn json_object(text: &str) -> Result<&str, String> {
    let start = text
        .find('{')
        .ok_or_else(|| "memory extraction returned no JSON object".to_string())?;
    let end = text
        .rfind('}')
        .ok_or_else(|| "memory extraction returned incomplete JSON".to_string())?;
    Ok(&text[start..=end])
}

fn user_attribution() -> String {
    "user".to_string()
}
