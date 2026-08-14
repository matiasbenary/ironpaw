mod near;
mod sqlite;

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use crate::{inference::Inference, message::Message, secrets::ToolSecrets};

use near::MemoryInference;
use sqlite::{MemoryRecord, SqliteMemory};

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub(super) struct CoreMemory {
    pub key: String,
    pub text: String,
}

pub struct Memory {
    store: SqliteMemory,
    inference: MemoryInference,
}

impl Memory {
    pub fn new(path: PathBuf, secrets: ToolSecrets, inference: Inference) -> Result<Self, String> {
        Ok(Self {
            store: SqliteMemory::new(path)?,
            inference: MemoryInference::new(secrets, inference)?,
        })
    }

    pub async fn recall(&self, query: &str) -> Result<Vec<String>, String> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let embedding = self.inference.embed(query).await?;
        let dense = self.store.search(&embedding, 20, 0.1)?;
        let lexical = self.store.search_text(query, 20)?;
        let mut scores = HashMap::<String, (f32, MemoryRecord)>::new();
        for results in [dense, lexical] {
            for (rank, memory) in results.into_iter().enumerate() {
                let score = 1.0 / (30.0 + rank as f32 + 1.0);
                scores
                    .entry(memory.id.clone())
                    .and_modify(|entry| entry.0 += score)
                    .or_insert((score, memory));
            }
        }
        let mut memories = scores.into_values().collect::<Vec<_>>();
        memories.sort_by(|left, right| right.0.total_cmp(&left.0));
        Ok(memories
            .into_iter()
            .take(10)
            .map(|(_, memory)| memory.text)
            .collect())
    }

    pub fn core(&self) -> Result<Vec<String>, String> {
        Ok(self
            .store
            .core(20)?
            .into_iter()
            .map(|memory| memory.text)
            .collect())
    }

    pub async fn ingest(
        &self,
        previous_messages: &[Message],
        new_messages: &[Message],
    ) -> Result<(), String> {
        let conversation = message_text(new_messages);
        if conversation.trim().is_empty() {
            return Ok(());
        }

        let query_embedding = self.inference.embed(&conversation).await?;
        let existing = self.store.search(&query_embedding, 10, 0.0)?;
        let core = self.store.core(20)?;
        let extracted = self
            .inference
            .extract(previous_messages, new_messages, &existing, &core)
            .await?;
        if extracted.memory.is_empty() && extracted.core_updates.is_empty() {
            return Ok(());
        }

        let known_hashes = self.store.hashes()?;
        let mut batch_hashes = HashSet::new();
        let mut memories = Vec::new();
        for extracted in extracted.memory {
            let text = extracted.text.trim().to_string();
            if text.is_empty() {
                continue;
            }
            let hash = text_hash(&text);
            if known_hashes.contains(&hash) || !batch_hashes.insert(hash.clone()) {
                continue;
            }
            memories.push(MemoryRecord {
                id: uuid::Uuid::new_v4().to_string(),
                text,
                embedding: Vec::new(),
                hash,
                attributed_to: extracted.attributed_to,
            });
        }

        let texts = memories
            .iter()
            .map(|memory| memory.text.as_str())
            .collect::<Vec<_>>();
        match self.inference.embed_batch(&texts).await {
            Ok(embeddings) => {
                for (memory, embedding) in memories.iter_mut().zip(embeddings) {
                    memory.embedding = embedding;
                }
            }
            Err(_) => {
                for memory in &mut memories {
                    if let Ok(embedding) = self.inference.embed(&memory.text).await {
                        memory.embedding = embedding;
                    }
                }
                memories.retain(|memory| !memory.embedding.is_empty());
            }
        }
        self.store.insert(&memories)?;
        let core_updates = extracted
            .core_updates
            .into_iter()
            .filter_map(|memory| {
                let key = memory.key.trim().to_string();
                let text = memory.text.trim().to_string();
                (!key.is_empty() && !text.is_empty()).then_some(CoreMemory { key, text })
            })
            .collect::<Vec<_>>();
        self.store.upsert_core(&core_updates)
    }
}

fn message_text(messages: &[Message]) -> String {
    messages
        .iter()
        .filter_map(|message| match message {
            Message::User { text, .. } => Some(format!("user: {text}")),
            Message::Assistant {
                text: Some(text), ..
            } => Some(format!("assistant: {text}")),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn text_hash(text: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(text.as_bytes()))
}
