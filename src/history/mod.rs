mod sqlite;

pub use sqlite::SqliteHistory;

use crate::{
    bus::{ConversationKey, MessageRef},
    message::Message,
};

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct HistoryEntry {
    pub message: Message,
    pub metadata: MessageMetadata,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct MessageMetadata {
    /// Warden-owned stable id for this history entry.
    pub id: String,
    pub conversation: ConversationKey,
    /// Provider-owned message id when Warden knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
}

impl MessageMetadata {
    pub fn new(conversation: ConversationKey, external_id: Option<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            conversation,
            external_id,
        }
    }
}

pub trait History: Send + Sync {
    /// Return active items for a conversation, oldest first.
    /// Inactive items stay on disk for search.
    fn load(&self, conversation: &ConversationKey) -> Vec<HistoryEntry>;

    /// Persist new items for a conversation.
    fn append(&self, conversation: &ConversationKey, entries: &[HistoryEntry]);

    /// Delete local entries that correspond to a channel/provider message id.
    fn delete_external_message(&self, message: &MessageRef);

    /// Replace a conversation with a summary item.
    fn compact(&self, conversation: &ConversationKey, summary: &str);

    /// Full-text search over all stored items.
    /// Returns matching items newest first.
    fn search(&self, query: &str) -> Vec<HistoryEntry>;
}
