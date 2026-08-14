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
pub enum HistoryId {
    Conversation(ConversationKey),
    Automation(String),
}

impl HistoryId {
    pub(crate) fn storage_key(&self) -> String {
        match self {
            Self::Conversation(conversation) => conversation.storage_key(),
            Self::Automation(automation_id) => format!("automation::{automation_id}"),
        }
    }
}

pub trait History: Send + Sync {
    /// Return the saved summary followed by messages written after its boundary.
    fn load_conversation(&self, history_id: &HistoryId) -> Result<Vec<HistoryEntry>, String>;

    fn append_history(
        &self,
        history_id: &HistoryId,
        entries: &[HistoryEntry],
    ) -> Result<(), String>;

    fn append_tool_turn(
        &self,
        history_id: &HistoryId,
        assistant: &Message,
        results: &[Message],
    ) -> Result<(), String> {
        if !matches!(assistant, Message::Assistant { .. }) {
            return Err("tool turn did not contain an assistant message".to_string());
        }
        if results
            .iter()
            .any(|message| !matches!(message, Message::ToolResult { .. }))
        {
            return Err("tool turn contained a non-tool result".to_string());
        }
        let messages =
            std::iter::once(assistant.clone()).chain(results.iter().map(bounded_history_message));
        let entries = messages
            .map(|message| HistoryEntry {
                message,
                metadata: metadata_for_history(history_id),
            })
            .collect::<Vec<_>>();
        self.append_history(history_id, &entries)
    }

    /// Replace the saved summary and advance its boundary to a persisted entry.
    fn save_conversation_summary(
        &self,
        history_id: &HistoryId,
        summary: &str,
        compacted_through_entry_id: &str,
    ) -> Result<(), String>;

    /// Return the conversation sent to the model, oldest first.
    fn load(&self, conversation: &ConversationKey) -> Result<Vec<HistoryEntry>, String> {
        self.load_conversation(&HistoryId::Conversation(conversation.clone()))
    }

    /// Persist new items for a conversation.
    fn append(
        &self,
        conversation: &ConversationKey,
        entries: &[HistoryEntry],
    ) -> Result<(), String> {
        self.append_history(&HistoryId::Conversation(conversation.clone()), entries)
    }

    /// Delete local entries that correspond to a channel/provider message id.
    fn delete_external_message(&self, message: &MessageRef) -> Result<(), String>;

    /// Full-text search over all stored items.
    /// Returns matching items newest first.
    fn search(&self, query: &str) -> Result<Vec<HistoryEntry>, String>;
}

fn metadata_for_history(history_id: &HistoryId) -> MessageMetadata {
    match history_id {
        HistoryId::Conversation(conversation) => MessageMetadata::new(conversation.clone(), None),
        HistoryId::Automation(automation_id) => {
            MessageMetadata::for_automation(automation_id.clone())
        }
    }
}

fn bounded_history_message(message: &Message) -> Message {
    const MAX_STRING_BYTES: usize = 8_000;
    const MAX_ARRAY_ITEMS: usize = 100;
    let Message::ToolResult {
        tool_call_id,
        tool_name,
        result,
    } = message
    else {
        return message.clone();
    };
    Message::ToolResult {
        tool_call_id: tool_call_id.clone(),
        tool_name: tool_name.clone(),
        result: bound_json(result, MAX_STRING_BYTES, MAX_ARRAY_ITEMS),
    }
}

fn bound_json(
    value: &serde_json::Value,
    max_string_bytes: usize,
    max_array_items: usize,
) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) if text.len() > max_string_bytes => {
            let mut end = max_string_bytes;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            serde_json::Value::String(format!("{}\n[truncated in durable history]", &text[..end]))
        }
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .take(max_array_items)
                .map(|item| bound_json(item, max_string_bytes, max_array_items))
                .collect(),
        ),
        serde_json::Value::Object(fields) => serde_json::Value::Object(
            fields
                .iter()
                .map(|(key, value)| {
                    (
                        key.clone(),
                        bound_json(value, max_string_bytes, max_array_items),
                    )
                })
                .collect(),
        ),
        _ => value.clone(),
    }
}
