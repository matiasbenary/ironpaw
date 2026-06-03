use std::{path::PathBuf, sync::Mutex};

use rusqlite::{Connection, params};

use crate::openai::{
    EasyInputContent, EasyInputMessage, FunctionCallOutput, InputContent, InputItem,
    Item, MessageItem, OutputMessageContent, Role,
};

use super::History;

/// SQLite-backed persistent history.
///
/// Schema:
///   entries(id, conversation_id, item_json, search_text, compacted)
///
/// `item_json`    — full serialised `InputItem`
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

        Ok(Self { conn: Mutex::new(conn) })
    }
}

fn extract_search_text(item: &InputItem) -> String {
    match item {
        InputItem::EasyMessage(m) => match &m.content {
            EasyInputContent::Text(t) => t.clone(),
            EasyInputContent::ContentList(parts) => parts
                .iter()
                .filter_map(|p| {
                    if let InputContent::InputText(t) = p {
                        Some(t.text.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join(" "),
        },
        InputItem::Item(item) => match item {
            Item::FunctionCall(call) => {
                format!("{}({})", call.name, call.arguments)
            }
            Item::FunctionCallOutput(out) => match &out.output {
                FunctionCallOutput::Text(t) => t.clone(),
                FunctionCallOutput::Content(_) => String::new(),
            },
            Item::Message(msg) => match msg {
                MessageItem::Output(m) => m
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        OutputMessageContent::OutputText(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
                MessageItem::Input(m) => m
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        InputContent::InputText(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
            },
            _ => String::new(),
        },
        InputItem::ItemReference(_) => String::new(),
    }
}

impl History for SqliteHistory {
    fn load(&self, conversation_id: &str) -> Vec<InputItem> {
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

    fn append(&self, conversation_id: &str, items: &[InputItem]) {
        let conn = self.conn.lock().expect("history mutex poisoned");
        for item in items {
            let json = serde_json::to_string(item).expect("serialize InputItem");
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
        let summary_item: InputItem = EasyInputMessage {
            r#type: Default::default(),
            role: Role::System,
            content: EasyInputContent::Text(format!("[conversation summary] {summary}")),
            phase: None,
        }
        .into();
        let json = serde_json::to_string(&summary_item).expect("serialize summary");
        conn.execute(
            "INSERT INTO entries(conversation_id, item_json, search_text)
             VALUES (?1, ?2, ?3)",
            params![conversation_id, json, summary],
        )
        .expect("insert summary");
    }

    fn search(&self, conversation_id: &str, query: &str) -> Vec<InputItem> {
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
        stmt.query_map(params![query, conversation_id], |row| row.get::<_, String>(0))
            .expect("query search")
            .filter_map(|r| r.ok())
            .filter_map(|json| serde_json::from_str(&json).ok())
            .collect()
    }
}
