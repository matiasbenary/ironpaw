use std::{path::PathBuf, sync::Mutex};

use rusqlite::{Connection, params};

use crate::message::Message;

use super::History;

/// SQLite-backed persistent history.
///
/// Schema:
///   entries(id, conversation_id, item_json, search_text, compacted)
///
/// `item_json`    — full serialised `Message`
/// `search_text`  — plain text extracted from the item, used for FTS5
/// `compacted`    — 1 when this entry has been replaced by a summary but is
///                  retained for `search()`. `load()` skips compacted entries.
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
                 conversation_id TEXT    NOT NULL,
                 item_json       TEXT    NOT NULL,
                 search_text     TEXT    NOT NULL DEFAULT '',
                 compacted       INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX IF NOT EXISTS idx_conv ON entries(conversation_id, compacted);
             CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(
                 search_text, content='entries', content_rowid='id'
             );
             CREATE TRIGGER IF NOT EXISTS entries_ai AFTER INSERT ON entries BEGIN
                 INSERT INTO entries_fts(rowid, search_text)
                 VALUES (new.id, new.search_text);
             END;",
        )
        .map_err(|e| e.to_string())?;

        Ok(Self {
            conn: Mutex::new(conn),
        })
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

impl History for SqliteHistory {
    fn load(&self, conversation_id: &str) -> Vec<Message> {
        let conn = self.conn.lock().expect("history mutex poisoned");
        let mut stmt = conn
            .prepare(
                "SELECT item_json FROM entries
                 WHERE conversation_id = ?1 AND compacted = 0
                 ORDER BY id ASC",
            )
            .expect("prepare load");
        stmt.query_map(params![conversation_id], |row| row.get::<_, String>(0))
            .expect("query load")
            .filter_map(|r| r.ok())
            .filter_map(|json| serde_json::from_str(&json).ok())
            .collect()
    }

    fn append(&self, conversation_id: &str, items: &[Message]) {
        let conn = self.conn.lock().expect("history mutex poisoned");
        for item in items {
            let json = serde_json::to_string(item).expect("serialize Message");
            let text = extract_search_text(item);
            conn.execute(
                "INSERT INTO entries(conversation_id, item_json, search_text)
                 VALUES (?1, ?2, ?3)",
                params![conversation_id, json, text],
            )
            .expect("insert entry");
        }
    }

    fn compact(&self, conversation_id: &str, summary: &str) {
        let conn = self.conn.lock().expect("history mutex poisoned");
        conn.execute(
            "UPDATE entries SET compacted = 1
             WHERE conversation_id = ?1 AND compacted = 0",
            params![conversation_id],
        )
        .expect("compact update");
        let summary_item = Message::System(format!("[conversation summary] {summary}"));
        let json = serde_json::to_string(&summary_item).expect("serialize summary");
        conn.execute(
            "INSERT INTO entries(conversation_id, item_json, search_text)
             VALUES (?1, ?2, ?3)",
            params![conversation_id, json, summary],
        )
        .expect("insert summary");
    }

    fn search(&self, conversation_id: &str, query: &str) -> Vec<Message> {
        let conn = self.conn.lock().expect("history mutex poisoned");
        let mut stmt = conn
            .prepare(
                "SELECT e.item_json
                 FROM entries_fts f
                 JOIN entries e ON e.id = f.rowid
                 WHERE f.search_text MATCH ?1
                   AND e.conversation_id = ?2
                 ORDER BY e.id DESC
                 LIMIT 20",
            )
            .expect("prepare search");
        stmt.query_map(params![query, conversation_id], |row| {
            row.get::<_, String>(0)
        })
        .expect("query search")
        .filter_map(|r| r.ok())
        .filter_map(|json| serde_json::from_str(&json).ok())
        .collect()
    }
}
