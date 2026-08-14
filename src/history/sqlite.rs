use std::{
    path::PathBuf,
    sync::{Mutex, MutexGuard},
};

use rusqlite::{Connection, OptionalExtension, params};

use crate::message::Message;

use super::{History, HistoryEntry, HistoryId, metadata_for_history};

/// SQLite-backed persistent history.
///
/// Schema:
///   entries(id, conversation_key, item_json, metadata_json, search_text)
///   conversation_summaries(history_key, summary, compacted_through_id)
///
/// `item_json`    — full serialised `Message`
/// `metadata_json` — full serialised `MessageMetadata`
/// `search_text`  — plain text extracted from the item, used for FTS5
pub struct SqliteHistory {
    conn: Mutex<Connection>,
}

impl SqliteHistory {
    pub fn new(path: PathBuf) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let conn = Connection::open(&path).map_err(|e| e.to_string())?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS entries (
                 id              INTEGER PRIMARY KEY,
                 conversation_key TEXT    NOT NULL,
                 item_json       TEXT    NOT NULL,
                 metadata_json   TEXT    NOT NULL DEFAULT '',
                 search_text     TEXT    NOT NULL DEFAULT ''
             );
             CREATE INDEX IF NOT EXISTS idx_history_entries
                 ON entries(conversation_key, id);
             CREATE TABLE IF NOT EXISTS conversation_summaries (
                 history_key         TEXT PRIMARY KEY,
                 summary             TEXT    NOT NULL,
                 compacted_through_id INTEGER NOT NULL
             );
             CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(
                 search_text, content='entries', content_rowid='id'
             );
             CREATE TRIGGER IF NOT EXISTS entries_ai AFTER INSERT ON entries BEGIN
                 INSERT INTO entries_fts(rowid, search_text)
                 VALUES (new.id, new.search_text);
             END;
             CREATE TRIGGER IF NOT EXISTS entries_ad AFTER DELETE ON entries BEGIN
                 INSERT INTO entries_fts(entries_fts, rowid, search_text)
                 VALUES ('delete', old.id, old.search_text);
             END;",
        )
        .map_err(|e| e.to_string())?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> Result<MutexGuard<'_, Connection>, String> {
        self.conn
            .lock()
            .map_err(|_| "history mutex poisoned".to_string())
    }
}

fn extract_search_text(item: &Message) -> String {
    match item {
        Message::System(text) => text.clone(),
        Message::User { text, attachments } => {
            let mut parts = vec![text.clone()];
            parts.extend(attachments.iter().map(|attachment| {
                format!(
                    "filename:{} mime_type:{}",
                    attachment.filename, attachment.mime_type,
                )
            }));
            parts.join(" ")
        }
        Message::Assistant {
            text,
            attachments,
            tool_calls,
        } => {
            let mut parts = text.clone().into_iter().collect::<Vec<_>>();
            parts.extend(attachments.iter().map(|attachment| {
                format!(
                    "filename:{} mime_type:{}",
                    attachment.filename, attachment.mime_type,
                )
            }));
            parts.extend(
                tool_calls
                    .iter()
                    .map(|call| format!("{}({})", call.name, call.arguments)),
            );
            parts.join(" ")
        }
        Message::ToolResult {
            tool_name, result, ..
        } => format!("{tool_name}: {result}"),
    }
}

fn entry_from_row(item_json: String, metadata_json: String) -> Option<HistoryEntry> {
    let message = serde_json::from_str(&item_json).ok()?;
    let metadata = serde_json::from_str(&metadata_json).ok()?;
    Some(HistoryEntry { message, metadata })
}

impl History for SqliteHistory {
    fn load_conversation(&self, history_id: &HistoryId) -> Result<Vec<HistoryEntry>, String> {
        let history_key = history_id.storage_key();
        let conn = self.conn()?;
        let saved = conn
            .query_row(
                "SELECT summary, compacted_through_id
                 FROM conversation_summaries WHERE history_key = ?1",
                params![&history_key],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        let (summary, compacted_through_id) = match saved {
            Some((summary, boundary)) => (Some(summary), boundary),
            None => (None, 0),
        };
        let mut stmt = conn
            .prepare(
                "SELECT item_json, metadata_json FROM entries
                 WHERE conversation_key = ?1
                   AND id > ?2
                 ORDER BY id ASC",
            )
            .map_err(|err| err.to_string())?;
        let rows = stmt
            .query_map(params![history_key, compacted_through_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|err| err.to_string())?;
        let mut entries = Vec::new();
        if let Some(summary) = summary {
            entries.push(HistoryEntry {
                message: Message::System(format!("[history summary] {summary}")),
                metadata: metadata_for_history(history_id),
            });
        }
        for row in rows {
            let (item_json, metadata_json) = row.map_err(|err| err.to_string())?;
            if let Some(entry) = entry_from_row(item_json, metadata_json) {
                entries.push(entry);
            }
        }
        Ok(entries)
    }

    fn append_history(
        &self,
        history_id: &HistoryId,
        entries: &[HistoryEntry],
    ) -> Result<(), String> {
        let conversation_key = history_id.storage_key();
        let conn = self.conn()?;
        for entry in entries {
            let json = serde_json::to_string(&entry.message).map_err(|err| err.to_string())?;
            let metadata_json =
                serde_json::to_string(&entry.metadata).map_err(|err| err.to_string())?;
            let text = extract_search_text(&entry.message);
            conn.execute(
                "INSERT INTO entries(conversation_key, item_json, metadata_json, search_text)
                 VALUES (?1, ?2, ?3, ?4)",
                params![conversation_key, json, metadata_json, text],
            )
            .map_err(|err| err.to_string())?;
        }
        Ok(())
    }

    fn delete_external_message(&self, message: &crate::bus::MessageRef) -> Result<(), String> {
        let conn = self.conn()?;
        match message.conversation.thread_id.as_deref() {
            Some(thread_id) => {
                conn.execute(
                    "DELETE FROM entries
                     WHERE json_valid(metadata_json)
                       AND json_extract(metadata_json, '$.conversation.channel') = ?1
                       AND json_extract(metadata_json, '$.conversation.chat_id') = ?2
                       AND json_extract(metadata_json, '$.conversation.thread_id') = ?3
                       AND json_extract(metadata_json, '$.external_id') = ?4",
                    params![
                        &message.conversation.channel,
                        &message.conversation.chat_id,
                        thread_id,
                        &message.message_id
                    ],
                )
                .map_err(|err| err.to_string())?;
            }
            None => {
                conn.execute(
                    "DELETE FROM entries
                     WHERE json_valid(metadata_json)
                       AND json_extract(metadata_json, '$.conversation.channel') = ?1
                       AND json_extract(metadata_json, '$.conversation.chat_id') = ?2
                       AND (
                           json_type(metadata_json, '$.conversation.thread_id') IS NULL
                           OR json_type(metadata_json, '$.conversation.thread_id') = 'null'
                       )
                       AND json_extract(metadata_json, '$.external_id') = ?3",
                    params![
                        &message.conversation.channel,
                        &message.conversation.chat_id,
                        &message.message_id
                    ],
                )
                .map_err(|err| err.to_string())?;
            }
        }
        Ok(())
    }

    fn save_conversation_summary(
        &self,
        history_id: &HistoryId,
        summary: &str,
        compacted_through_entry_id: &str,
    ) -> Result<(), String> {
        let history_key = history_id.storage_key();
        let mut conn = self.conn()?;
        let transaction = conn.transaction().map_err(|err| err.to_string())?;
        let boundary = transaction
            .query_row(
                "SELECT id FROM entries
                 WHERE conversation_key = ?1
                   AND json_extract(metadata_json, '$.id') = ?2",
                params![&history_key, compacted_through_entry_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "conversation summary boundary was not found".to_string())?;
        transaction
            .execute(
                "INSERT INTO conversation_summaries(
                     history_key, summary, compacted_through_id
                 ) VALUES (?1, ?2, ?3)
                 ON CONFLICT(history_key) DO UPDATE SET
                     summary = excluded.summary,
                     compacted_through_id = excluded.compacted_through_id
                 WHERE excluded.compacted_through_id
                     > conversation_summaries.compacted_through_id",
                params![&history_key, summary, boundary],
            )
            .map_err(|err| err.to_string())?;
        transaction.commit().map_err(|err| err.to_string())
    }

    fn search(&self, query: &str) -> Result<Vec<HistoryEntry>, String> {
        let conn = self.conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT e.item_json, e.metadata_json
                 FROM entries_fts f
                 JOIN entries e ON e.id = f.rowid
                 WHERE f.search_text MATCH ?1
                   AND NOT EXISTS (
                     SELECT 1
                     FROM json_each(e.item_json, '$.Assistant.tool_calls') call
                     WHERE json_extract(call.value, '$.name') = 'search_history'
                   )
                 ORDER BY bm25(entries_fts)
                 LIMIT 20",
            )
            .map_err(|err| err.to_string())?;
        let rows = stmt
            .query_map(params![query], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|err| err.to_string())?;
        let mut entries = Vec::new();
        for row in rows {
            let (item_json, metadata_json) = row.map_err(|err| err.to_string())?;
            if let Some(entry) = entry_from_row(item_json, metadata_json) {
                entries.push(entry);
            }
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{ConversationKey, MessageRef};
    use crate::history::MessageMetadata;

    fn temp_history() -> SqliteHistory {
        let path =
            std::env::temp_dir().join(format!("ironpaw-history-test-{}.db", uuid::Uuid::new_v4()));
        SqliteHistory::new(path).unwrap()
    }

    #[test]
    fn appends_a_complete_bounded_tool_turn() {
        let history = temp_history();
        let history_id = HistoryId::Automation("daily-album".to_string());
        let assistant = Message::Assistant {
            text: Some("checking".to_string()),
            attachments: Vec::new(),
            tool_calls: vec![crate::message::ToolCall {
                id: "call-1".to_string(),
                name: "web_fetch".to_string(),
                arguments: "{}".to_string(),
            }],
        };
        let result = Message::ToolResult {
            tool_call_id: "call-1".to_string(),
            tool_name: "web_fetch".to_string(),
            result: serde_json::json!({ "body": "x".repeat(9_000) }),
        };

        history
            .append_tool_turn(&history_id, &assistant, &[result])
            .unwrap();

        let entries = history.load_conversation(&history_id).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].message, assistant);
        let Message::ToolResult { result, .. } = &entries[1].message else {
            panic!("expected tool result");
        };
        assert!(
            result["body"]
                .as_str()
                .unwrap()
                .contains("truncated in durable history")
        );
    }

    #[test]
    fn conversation_is_summary_followed_by_entries_after_the_boundary() {
        let history = temp_history();
        let history_id = HistoryId::Automation("daily-album".to_string());
        let entries = vec![
            HistoryEntry {
                message: Message::User {
                    text: "old request".to_string(),
                    attachments: Vec::new(),
                },
                metadata: MessageMetadata::for_automation("daily-album"),
            },
            HistoryEntry {
                message: Message::User {
                    text: "current request".to_string(),
                    attachments: Vec::new(),
                },
                metadata: MessageMetadata::for_automation("daily-album"),
            },
        ];
        history.append_history(&history_id, &entries).unwrap();

        history
            .save_conversation_summary(&history_id, "older context", &entries[0].metadata.id)
            .unwrap();

        let loaded = history.load_conversation(&history_id).unwrap();
        assert!(
            matches!(&loaded[0].message, Message::System(text) if text.contains("older context"))
        );
        assert_eq!(loaded[1], entries[1]);
    }

    #[test]
    fn deletes_by_channel_conversation_thread_and_external_id() {
        let history = temp_history();
        let thread_1 = ConversationKey {
            channel: "telegram".to_string(),
            chat_id: "chat-1".to_string(),
            thread_id: Some("thread-1".to_string()),
        };
        let thread_2 = ConversationKey {
            channel: "telegram".to_string(),
            chat_id: "chat-1".to_string(),
            thread_id: Some("thread-2".to_string()),
        };
        history
            .append(
                &thread_1,
                &[HistoryEntry {
                    message: Message::User {
                        text: "secret".to_string(),
                        attachments: Vec::new(),
                    },
                    metadata: MessageMetadata::new(
                        thread_1.clone(),
                        Some("provider-1".to_string()),
                    ),
                }],
            )
            .unwrap();
        history
            .append(
                &thread_2,
                &[HistoryEntry {
                    message: Message::User {
                        text: "keep".to_string(),
                        attachments: Vec::new(),
                    },
                    metadata: MessageMetadata::new(
                        thread_2.clone(),
                        Some("provider-1".to_string()),
                    ),
                }],
            )
            .unwrap();

        history
            .delete_external_message(&MessageRef {
                conversation: thread_1,
                message_id: "provider-1".to_string(),
            })
            .unwrap();

        let loaded = history.load(&thread_2).unwrap();
        assert_eq!(loaded.len(), 1);
        let Message::User { text, .. } = &loaded[0].message else {
            panic!("expected user message");
        };
        assert_eq!(text, "keep");
    }
}
