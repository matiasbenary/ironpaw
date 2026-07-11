use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Mutex, MutexGuard},
};

use rusqlite::{Connection, params};

#[derive(Clone)]
pub(super) struct MemoryRecord {
    pub id: String,
    pub text: String,
    pub embedding: Vec<f32>,
    pub hash: String,
    pub attributed_to: String,
}

pub(super) struct SqliteMemory {
    conn: Mutex<Connection>,
}

impl SqliteMemory {
    pub fn new(path: PathBuf) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let conn = Connection::open(path).map_err(|error| error.to_string())?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS memories (
                 id            TEXT PRIMARY KEY,
                 text          TEXT NOT NULL,
                 embedding     BLOB NOT NULL,
                 hash          TEXT NOT NULL UNIQUE,
                 attributed_to TEXT NOT NULL,
                 created_at    TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
             );
             CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
                 id UNINDEXED,
                 text
             );
             DELETE FROM memories_fts;
             INSERT INTO memories_fts(id, text) SELECT id, text FROM memories;",
        )
        .map_err(|error| error.to_string())?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn insert(&self, memories: &[MemoryRecord]) -> Result<(), String> {
        let mut conn = self.conn()?;
        let transaction = conn.transaction().map_err(|error| error.to_string())?;
        for memory in memories {
            let inserted = transaction
                .execute(
                    "INSERT OR IGNORE INTO memories(id, text, embedding, hash, attributed_to)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        memory.id,
                        memory.text,
                        encode_embedding(&memory.embedding),
                        memory.hash,
                        memory.attributed_to,
                    ],
                )
                .map_err(|error| error.to_string())?;
            if inserted > 0 {
                transaction
                    .execute(
                        "INSERT INTO memories_fts(id, text) VALUES (?1, ?2)",
                        params![memory.id, memory.text],
                    )
                    .map_err(|error| error.to_string())?;
            }
        }
        transaction.commit().map_err(|error| error.to_string())
    }

    pub fn hashes(&self) -> Result<HashSet<String>, String> {
        let conn = self.conn()?;
        let mut statement = conn
            .prepare("SELECT hash FROM memories")
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| row.get(0))
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<HashSet<_>, _>>()
            .map_err(|error| error.to_string())
    }

    pub fn search(
        &self,
        query: &[f32],
        limit: usize,
        threshold: f32,
    ) -> Result<Vec<MemoryRecord>, String> {
        let conn = self.conn()?;
        let mut statement = conn
            .prepare("SELECT id, text, embedding, hash, attributed_to FROM memories")
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok(MemoryRecord {
                    id: row.get(0)?,
                    text: row.get(1)?,
                    embedding: decode_embedding(&row.get::<_, Vec<u8>>(2)?),
                    hash: row.get(3)?,
                    attributed_to: row.get(4)?,
                })
            })
            .map_err(|error| error.to_string())?;
        let mut matches = rows
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?
            .into_iter()
            .filter_map(|memory| {
                let score = cosine_similarity(query, &memory.embedding);
                (score >= threshold).then_some((score, memory))
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| right.0.total_cmp(&left.0));
        Ok(matches
            .into_iter()
            .take(limit)
            .map(|(_, memory)| memory)
            .collect())
    }

    pub fn search_text(&self, query: &str, limit: usize) -> Result<Vec<MemoryRecord>, String> {
        let terms = query
            .split(|character: char| !character.is_alphanumeric())
            .filter(|term| term.len() > 1)
            .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
            .collect::<Vec<_>>();
        if terms.is_empty() {
            return Ok(Vec::new());
        }

        let conn = self.conn()?;
        let mut statement = conn
            .prepare(
                "SELECT m.id, m.text, m.embedding, m.hash, m.attributed_to
                 FROM memories_fts f
                 JOIN memories m ON m.id = f.id
                 WHERE memories_fts MATCH ?1
                 ORDER BY bm25(memories_fts)
                 LIMIT ?2",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map(params![terms.join(" OR "), limit], |row| {
                Ok(MemoryRecord {
                    id: row.get(0)?,
                    text: row.get(1)?,
                    embedding: decode_embedding(&row.get::<_, Vec<u8>>(2)?),
                    hash: row.get(3)?,
                    attributed_to: row.get(4)?,
                })
            })
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    fn conn(&self) -> Result<MutexGuard<'_, Connection>, String> {
        self.conn
            .lock()
            .map_err(|_| "memory mutex poisoned".to_string())
    }
}

fn encode_embedding(embedding: &[f32]) -> Vec<u8> {
    embedding
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn decode_embedding(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes(chunk.try_into().expect("four byte chunk")))
        .collect()
}

fn cosine_similarity(left: &[f32], right: &[f32]) -> f32 {
    if left.len() != right.len() || left.is_empty() {
        return 0.0;
    }
    let dot = left.iter().zip(right).map(|(a, b)| a * b).sum::<f32>();
    let left_norm = left.iter().map(|value| value * value).sum::<f32>().sqrt();
    let right_norm = right.iter().map(|value| value * value).sum::<f32>().sqrt();
    if left_norm == 0.0 || right_norm == 0.0 {
        0.0
    } else {
        dot / (left_norm * right_norm)
    }
}

#[cfg(test)]
mod tests {
    use super::{MemoryRecord, SqliteMemory, cosine_similarity};

    #[test]
    fn cosine_similarity_orders_related_vectors() {
        assert!(cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) > 0.99);
        assert_eq!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
    }

    #[test]
    fn text_search_preserves_exact_identifiers() {
        let path = std::env::temp_dir().join(format!("warden-memory-{}.db", uuid::Uuid::new_v4()));
        let store = SqliteMemory::new(path).unwrap();
        store
            .insert(&[MemoryRecord {
                id: "hub".into(),
                text: "The Lumio Hub v2 runs firmware 2.7.4".into(),
                embedding: vec![1.0],
                hash: "hash".into(),
                attributed_to: "user".into(),
            }])
            .unwrap();

        let matches = store.search_text("Lumio Hub v2 2.7.4", 10).unwrap();
        assert_eq!(matches[0].id, "hub");
    }
}
