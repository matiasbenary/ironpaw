use std::{collections::HashSet, sync::Arc};

use crate::{
    message::Message,
    models,
    tools::raw_inference::{RawInferenceRequest, RawInferenceTool},
};

use super::{History, HistoryEntry, HistoryScope};

const COMPACT_AT_PROMPT_TOKENS: u64 = 256_000;

/// Owns the model-facing projection of immutable history.
pub struct HistoryContext {
    history: Arc<dyn History>,
    raw_inference: RawInferenceTool,
}

impl HistoryContext {
    pub fn new(history: Arc<dyn History>, raw_inference: RawInferenceTool) -> Self {
        Self {
            history,
            raw_inference,
        }
    }

    pub fn load(&self, scope: &HistoryScope) -> Result<Vec<HistoryEntry>, String> {
        self.history.load_scope(scope)
    }

    pub fn append(&self, scope: &HistoryScope, entries: &[HistoryEntry]) -> Result<(), String> {
        self.history.append_scope(scope, entries)
    }

    pub async fn compact_if_needed(
        &self,
        scope: &HistoryScope,
        prompt_tokens: u64,
    ) -> Result<(), String> {
        if prompt_tokens >= COMPACT_AT_PROMPT_TOKENS {
            self.compact_active_context(scope).await?;
        }
        Ok(())
    }

    pub async fn compact(&self, scope: &HistoryScope) -> Result<(), String> {
        let messages = self
            .load(scope)?
            .into_iter()
            .map(|entry| entry.message)
            .collect::<Vec<_>>();
        if messages.is_empty() {
            return Ok(());
        }
        let summary = self
            .summarize(
                messages,
                "Summarize this history for future context. Preserve exact prior actions, deliveries, recommendations, outcomes, files changed, errors, unresolved work, names, identifiers, and dates. Collapse raw tool results and intermediate reasoning once their relevant outcome is captured. Do not invent or infer unsupported events.",
            )
            .await?;
        self.history.compact_scope(scope, &summary, &[])
    }

    async fn compact_active_context(&self, scope: &HistoryScope) -> Result<(), String> {
        let history = self.load(scope)?;
        if history.is_empty() {
            return Ok(());
        }
        let retained = retained_entries(&history);
        let retained_ids = retained
            .iter()
            .map(|entry| entry.metadata.id.as_str())
            .collect::<HashSet<_>>();
        let messages = history
            .iter()
            .filter(|entry| !retained_ids.contains(entry.metadata.id.as_str()))
            .map(|entry| entry.message.clone())
            .collect::<Vec<_>>();
        if messages.is_empty() {
            return Ok(());
        }
        let summary = self
            .summarize(
                messages,
                "Summarize older history for future runs. This summary is not an action ledger. Do not claim an action, delivery, user acknowledgement, recommendation, date, or research finding unless the transcript explicitly proves it. Omit uncertain information. Preserve only user preferences, durable source ids, verified tool evidence, and unresolved work.",
            )
            .await?;
        self.history.compact_scope(scope, &summary, &retained)
    }

    async fn summarize(&self, messages: Vec<Message>, prompt: &str) -> Result<String, String> {
        let transcript = serde_json::to_string(&messages).map_err(|err| err.to_string())?;
        let response = self
            .raw_inference
            .execute(RawInferenceRequest {
                model: models::text_model(),
                messages: vec![
                    Message::System(prompt.to_string()),
                    Message::User {
                        text: transcript,
                        attachments: Vec::new(),
                    },
                ],
                tools: Vec::new(),
                parallel_tool_calls: false,
            })
            .await?;
        let Message::Assistant { text, .. } = response.message else {
            return Err(
                "history compaction model response was not an assistant message".to_string(),
            );
        };
        let summary = text.unwrap_or_default();
        if summary.trim().is_empty() {
            return Err("history compaction response was empty".to_string());
        }
        Ok(summary)
    }
}

fn retained_entries(history: &[HistoryEntry]) -> Vec<HistoryEntry> {
    let mut retained = history.iter().rev().take(4).cloned().collect::<Vec<_>>();
    if let Some(user) = history
        .iter()
        .rev()
        .find(|entry| matches!(entry.message, Message::User { .. }))
        && !retained
            .iter()
            .any(|entry| entry.metadata.id == user.metadata.id)
    {
        retained.push(user.clone());
    }
    retained.reverse();
    retained
}
