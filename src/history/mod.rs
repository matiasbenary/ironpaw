mod sqlite;

pub use sqlite::SqliteHistory;

use crate::message::Message;

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct HistoryEntry {
    pub message: Message,
    pub metadata: MessageMetadata,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct MessageMetadata {
    /// Warden-owned stable id for this history entry.
    pub id: String,
    /// Channel that produced or receives this message, e.g. "telegram".
    pub channel: String,
    /// Channel conversation/chat id.
    pub conversation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// Provider-owned message id when Warden knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
}

impl MessageMetadata {
    pub fn new(
        channel: impl Into<String>,
        conversation_id: impl Into<String>,
        thread_id: Option<String>,
        external_id: Option<String>,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            channel: channel.into(),
            conversation_id: conversation_id.into(),
            thread_id,
            external_id,
        }
    }
}

pub trait History: Send + Sync {
    /// Return active items for a conversation, oldest first.
    /// Inactive items stay on disk for search.
    fn load(&self, conversation_id: &str) -> Vec<HistoryEntry>;

    /// Persist new items for a conversation.
    fn append(&self, conversation_id: &str, entries: &[HistoryEntry]);

    /// Delete local entries that correspond to a channel/provider message id.
    fn delete_external_message(
        &self,
        channel: &str,
        conversation_id: &str,
        thread_id: Option<&str>,
        external_id: &str,
    );

    /// Replace a conversation with a summary item.
    fn compact(&self, conversation_id: &str, summary: &str);

    /// Full-text search over all stored items.
    /// Returns matching items newest first.
    fn search(&self, query: &str) -> Vec<HistoryEntry>;
}
