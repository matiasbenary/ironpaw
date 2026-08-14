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
    inference: Inference,
}

impl MemoryInference {
    pub fn new(_secrets: ToolSecrets, inference: Inference) -> Result<Self, String> {
        Ok(Self { inference })
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
            .execute(InferenceRequest {
                model: self.inference.provider().text_model.clone(),
                messages: vec![
                    Message::System(EXTRACTION_PROMPT.to_string()),
                    Message::User {
                        text: input.to_string(),
                        attachments: Vec::new(),
                    },
                ],
                tools: Vec::new(),
            })
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
        let response = self
            .inference
            .post_json(
                "/v1/embeddings",
                &serde_json::json!({
                    "model": &self.inference.provider().embedding_model,
                    "input": texts,
                    "encoding_format": "float",
                }),
            )
            .await
            .map_err(|error| error.to_string())?;
        if !(200..300).contains(&response.status) {
            return Err(format!(
                "NEAR embeddings failed with {}: {}",
                response.status, response.body
            ));
        }
        let mut response: EmbeddingResponse =
            serde_json::from_str(&response.body).map_err(|error| error.to_string())?;
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
