mod sqlite;

pub use sqlite::SqliteHistory;

use crate::message::Message;

pub trait History: Send + Sync {
    /// Return stored items for a conversation, oldest first.
    /// Non-compacted items only — compacted items are retained for search.
    fn load(&self, conversation_id: &str) -> Vec<Message>;

    /// Persist new items for a conversation.
    fn append(&self, conversation_id: &str, items: &[Message]);

    /// Replace all active items with a compacted summary item, retaining
    /// the originals on disk (compacted=1) so they remain searchable.
    fn compact(&self, conversation_id: &str, summary: &str);

    /// Full-text search over all stored items (including compacted).
    /// Returns matching items newest first.
    fn search(&self, conversation_id: &str, query: &str) -> Vec<Message>;
}
