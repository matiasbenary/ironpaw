use std::{
    path::PathBuf,
    sync::{Mutex, MutexGuard},
};

use rusqlite::{Connection, params};

use crate::message::Message;

use super::{History, HistoryEntry, HistoryScope, MessageMetadata};

/// SQLite-backed persistent history.
///
/// Schema:
///   entries(id, conversation_key, item_json, metadata_json, search_text, active)
///
/// `item_json`    — full serialised `Message`
/// `metadata_json` — full serialised `MessageMetadata`
/// `search_text`  — plain text extracted from the item, used for FTS5
/// `active`       — 1 for messages loaded into the live context, 0 for cold
///                  storage retained for search
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
                 search_text     TEXT    NOT NULL DEFAULT '',
                 active          INTEGER NOT NULL DEFAULT 1
             );
             CREATE INDEX IF NOT EXISTS idx_conv ON entries(conversation_key, active);
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
                    "file_id:{} mime_type:{} {}",
                    attachment.file_id,
                    attachment.mime_type,
                    attachment.filename.as_deref().unwrap_or("")
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
                    "file_id:{} mime_type:{} {}",
                    attachment.file_id,
                    attachment.mime_type,
                    attachment.filename.as_deref().unwrap_or("")
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
    fn load_scope(&self, scope: &HistoryScope) -> Result<Vec<HistoryEntry>, String> {
        let conversation_key = scope.storage_key();
        let conn = self.conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT item_json, metadata_json FROM entries
                 WHERE conversation_key = ?1 AND active = 1
                 ORDER BY CASE
                     WHEN json_extract(item_json, '$.System') LIKE '[history summary] %' THEN 0
                     ELSE 1
                 END, id ASC",
            )
            .map_err(|err| err.to_string())?;
        let rows = stmt
            .query_map(params![conversation_key], |row| {
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

    fn append_scope(&self, scope: &HistoryScope, entries: &[HistoryEntry]) -> Result<(), String> {
        let conversation_key = scope.storage_key();
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

    fn compact_scope(
        &self,
        scope: &HistoryScope,
        summary: &str,
        retained: &[HistoryEntry],
    ) -> Result<(), String> {
        let conversation_key = scope.storage_key();
        let mut conn = self.conn()?;
        let transaction = conn.transaction().map_err(|err| err.to_string())?;
        transaction
            .execute(
                "UPDATE entries SET active = 0 WHERE conversation_key = ?1 AND active = 1",
                params![&conversation_key],
            )
            .map_err(|err| err.to_string())?;
        let summary_item = Message::System(format!("[history summary] {summary}"));
        let summary_metadata = match scope {
            HistoryScope::Conversation(conversation) => {
                MessageMetadata::new(conversation.clone(), None)
            }
            HistoryScope::Automation(automation_id) => {
                MessageMetadata::for_automation(automation_id.clone())
            }
        };
        let json = serde_json::to_string(&summary_item).map_err(|err| err.to_string())?;
        let metadata_json =
            serde_json::to_string(&summary_metadata).map_err(|err| err.to_string())?;
        transaction
            .execute(
                "INSERT INTO entries(conversation_key, item_json, metadata_json, search_text)
             VALUES (?1, ?2, ?3, ?4)",
                params![&conversation_key, json, metadata_json, summary],
            )
            .map_err(|err| err.to_string())?;
        for entry in retained {
            transaction
                .execute(
                    "UPDATE entries SET active = 1
                     WHERE conversation_key = ?1
                       AND json_extract(metadata_json, '$.id') = ?2",
                    params![&conversation_key, &entry.metadata.id],
                )
                .map_err(|err| err.to_string())?;
        }
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

    fn temp_history() -> SqliteHistory {
        let path =
            std::env::temp_dir().join(format!("warden-history-test-{}.db", uuid::Uuid::new_v4()));
        SqliteHistory::new(path).unwrap()
    }

    #[test]
    fn appends_and_loads_message_metadata() {
        let history = temp_history();
        let conversation = ConversationKey {
            channel: "telegram".to_string(),
            chat_id: "chat-1".to_string(),
            thread_id: Some("thread-1".to_string()),
        };
        let entry = HistoryEntry {
            message: Message::User {
                text: "hello".to_string(),
                attachments: Vec::new(),
            },
            metadata: MessageMetadata::new(conversation.clone(), Some("provider-1".to_string())),
        };

        history
            .append(&conversation, std::slice::from_ref(&entry))
            .unwrap();

        let loaded = history.load(&conversation).unwrap();
        assert_eq!(loaded, vec![entry]);
    }

    #[test]
    fn metadata_external_id_can_be_missing() {
        let history = temp_history();
        let conversation = ConversationKey {
            channel: "telegram".to_string(),
            chat_id: "chat-1".to_string(),
            thread_id: None,
        };
        history
            .append(
                &conversation,
                &[HistoryEntry {
                    message: Message::Assistant {
                        text: Some("done".to_string()),
                        attachments: Vec::new(),
                        tool_calls: Vec::new(),
                    },
                    metadata: MessageMetadata::new(conversation.clone(), None),
                }],
            )
            .unwrap();

        let loaded = history.load(&conversation).unwrap();
        assert_eq!(loaded.len(), 1);
        let loaded_conversation = loaded[0].metadata.conversation.as_ref().unwrap();
        assert_eq!(loaded_conversation.channel, "telegram");
        assert_eq!(loaded_conversation.chat_id, "chat-1");
        assert_eq!(loaded[0].metadata.external_id, None);
    }

    #[test]
    fn automation_history_is_durable_and_isolated() {
        let history = temp_history();
        let daily_album = HistoryScope::Automation("daily-album".to_string());
        let daily_news = HistoryScope::Automation("daily-news".to_string());
        let entry = HistoryEntry {
            message: Message::Assistant {
                text: Some("recommended album".to_string()),
                attachments: Vec::new(),
                tool_calls: Vec::new(),
            },
            metadata: MessageMetadata::for_automation("daily-album"),
        };

        history
            .append_scope(&daily_album, std::slice::from_ref(&entry))
            .unwrap();

        assert_eq!(history.load_scope(&daily_album).unwrap(), vec![entry]);
        assert!(history.load_scope(&daily_news).unwrap().is_empty());
    }

    #[test]
    fn compaction_retains_active_task_context() {
        let history = temp_history();
        let scope = HistoryScope::Automation("daily-album".to_string());
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
        history.append_scope(&scope, &entries).unwrap();

        history
            .compact_scope(&scope, "older context", &entries[1..])
            .unwrap();

        let loaded = history.load_scope(&scope).unwrap();
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
