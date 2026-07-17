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
    /// Ironpaw-owned stable id for this history entry.
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation: Option<ConversationKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automation_id: Option<String>,
    /// Provider-owned message id when Ironpaw knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
}

impl MessageMetadata {
    pub fn new(conversation: ConversationKey, external_id: Option<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            conversation: Some(conversation),
            automation_id: None,
            external_id,
        }
    }

    pub fn for_automation(automation_id: impl Into<String>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            conversation: None,
            automation_id: Some(automation_id.into()),
            external_id: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryScope {
    Conversation(ConversationKey),
    Automation(String),
}

impl HistoryScope {
    pub(crate) fn storage_key(&self) -> String {
        match self {
            Self::Conversation(conversation) => conversation.storage_key(),
            Self::Automation(automation_id) => format!("automation::{automation_id}"),
        }
    }
}

pub trait History: Send + Sync {
    fn load_scope(&self, scope: &HistoryScope) -> Result<Vec<HistoryEntry>, String>;

    fn append_scope(&self, scope: &HistoryScope, entries: &[HistoryEntry]) -> Result<(), String>;

    fn compact_scope(
        &self,
        scope: &HistoryScope,
        summary: &str,
        retained: &[HistoryEntry],
    ) -> Result<(), String>;

    /// Return active items for a conversation, oldest first.
    /// Inactive items stay on disk for search.
    fn load(&self, conversation: &ConversationKey) -> Result<Vec<HistoryEntry>, String> {
        self.load_scope(&HistoryScope::Conversation(conversation.clone()))
    }

    /// Persist new items for a conversation.
    fn append(
        &self,
        conversation: &ConversationKey,
        entries: &[HistoryEntry],
    ) -> Result<(), String> {
        self.append_scope(&HistoryScope::Conversation(conversation.clone()), entries)
    }

    /// Delete local entries that correspond to a channel/provider message id.
    fn delete_external_message(&self, message: &MessageRef) -> Result<(), String>;

    /// Replace a conversation with a summary item.
    fn compact(&self, conversation: &ConversationKey, summary: &str) -> Result<(), String> {
        self.compact_scope(
            &HistoryScope::Conversation(conversation.clone()),
            summary,
            &[],
        )
    }

    /// Full-text search over all stored items.
    /// Returns matching items newest first.
    fn search(&self, query: &str) -> Result<Vec<HistoryEntry>, String>;
}
