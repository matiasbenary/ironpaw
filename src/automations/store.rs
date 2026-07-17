use std::{
    path::PathBuf,
    sync::{Mutex, MutexGuard},
};

use rusqlite::{Connection, params};

use super::{Automation, AutomationEffect, Trigger};

pub struct SqliteAutomations {
    conn: Mutex<Connection>,
}

impl SqliteAutomations {
    pub fn new(path: PathBuf) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
        let conn = Connection::open(path).map_err(|err| err.to_string())?;
        drop_old_schema(&conn)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS automations (
                 name         TEXT    PRIMARY KEY,
                 trigger_json TEXT    NOT NULL,
                 effect_json  TEXT    NOT NULL,
                 created_at   TEXT    NOT NULL
             );",
        )
        .map_err(|err| err.to_string())?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn insert(&self, automation: &Automation) -> Result<(), String> {
        let trigger_json =
            serde_json::to_string(&automation.trigger).map_err(|err| err.to_string())?;
        let effect_json =
            serde_json::to_string(&automation.effect).map_err(|err| err.to_string())?;
        let created_at = chrono::Utc::now().to_rfc3339();
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO automations(name, trigger_json, effect_json, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![&automation.name, trigger_json, effect_json, created_at,],
        )
        .map(|_| ())
        .map_err(|err| {
            if matches!(err, rusqlite::Error::SqliteFailure(ref code, _)
                if code.code == rusqlite::ErrorCode::ConstraintViolation)
            {
                format!("an automation named '{}' already exists", automation.name)
            } else {
                err.to_string()
            }
        })
    }

    pub fn delete(&self, name: &str) -> Result<(), String> {
        let conn = self.conn()?;
        conn.execute("DELETE FROM automations WHERE name = ?1", params![name])
            .map(|_| ())
            .map_err(|err| err.to_string())
    }

    pub fn load_all(&self) -> Result<Vec<Automation>, String> {
        let conn = self.conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT name, trigger_json, effect_json
                 FROM automations
                 ORDER BY created_at ASC, name ASC",
            )
            .map_err(|err| err.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok(StoredAutomationRow {
                    name: row.get(0)?,
                    trigger_json: row.get(1)?,
                    effect_json: row.get(2)?,
                })
            })
            .map_err(|err| err.to_string())?;

        let mut automations = Vec::new();
        for row in rows {
            let row = row.map_err(|err| err.to_string())?;
            automations.push(row.into_automation()?);
        }
        Ok(automations)
    }

    fn conn(&self) -> Result<MutexGuard<'_, Connection>, String> {
        self.conn
            .lock()
            .map_err(|_| "automations mutex poisoned".to_string())
    }
}

fn drop_old_schema(conn: &Connection) -> Result<(), String> {
    let exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'automations'",
            [],
            |row| row.get(0),
        )
        .map_err(|err| err.to_string())?;
    if exists == 0 {
        return Ok(());
    }
    let mut stmt = conn
        .prepare("PRAGMA table_info(automations)")
        .map_err(|err| err.to_string())?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    if !columns.iter().any(|column| column == "effect_json") {
        conn.execute("DROP TABLE automations", [])
            .map_err(|err| err.to_string())?;
    }
    Ok(())
}

struct StoredAutomationRow {
    name: String,
    trigger_json: String,
    effect_json: String,
}

impl StoredAutomationRow {
    fn into_automation(self) -> Result<Automation, String> {
        let trigger: Trigger =
            serde_json::from_str(&self.trigger_json).map_err(|err| err.to_string())?;
        let effect: AutomationEffect =
            serde_json::from_str(&self.effect_json).map_err(|err| err.to_string())?;
        Ok(Automation {
            name: self.name,
            trigger,
            effect,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{ConversationKey, SenderKind};

    fn temp_store() -> SqliteAutomations {
        let path = std::env::temp_dir().join(format!(
            "ironpaw-automations-test-{}.db",
            uuid::Uuid::new_v4()
        ));
        SqliteAutomations::new(path).expect("store")
    }

    fn send_message_automation(name: &str, trigger: Trigger) -> Automation {
        Automation {
            name: name.to_string(),
            trigger,
            effect: AutomationEffect::Tool {
                tool: "send_message".to_string(),
                args: serde_json::json!({
                    "channel": "tui",
                    "chat_id": "c1",
                    "content": "hi",
                }),
            },
        }
    }

    #[test]
    fn sqlite_store_round_trips_automations() {
        let store = temp_store();
        let at_time = (chrono::Local::now() + chrono::Duration::hours(1)).to_rfc3339();
        store
            .insert(&send_message_automation(
                "daily-news",
                Trigger::cron("0 9 * * *").unwrap(),
            ))
            .unwrap();
        store
            .insert(&send_message_automation(
                "later",
                Trigger::at(&at_time).unwrap(),
            ))
            .unwrap();
        store
            .insert(&send_message_automation(
                "telegram-in",
                Trigger::MessageIn {
                    sender: SenderKind::User,
                    conversation: Some(ConversationKey {
                        channel: "telegram".to_string(),
                        chat_id: "c1".to_string(),
                        thread_id: None,
                    }),
                },
            ))
            .unwrap();

        let loaded = store.load_all().unwrap();

        assert_eq!(loaded.len(), 3);
        let daily = loaded
            .iter()
            .find(|automation| automation.name == "daily-news")
            .expect("daily-news");
        let Trigger::Cron { expression, .. } = &daily.trigger else {
            panic!("expected cron");
        };
        assert_eq!(expression, "0 0 9 * * *");
        let later = loaded
            .iter()
            .find(|automation| automation.name == "later")
            .expect("later");
        let Trigger::At { datetime } = &later.trigger else {
            panic!("expected at");
        };
        assert_eq!(datetime.to_rfc3339(), at_time);
        let telegram = loaded
            .iter()
            .find(|automation| automation.name == "telegram-in")
            .expect("telegram-in");
        let Trigger::MessageIn {
            sender,
            conversation,
        } = &telegram.trigger
        else {
            panic!("expected message_in");
        };
        assert_eq!(sender, &SenderKind::User);
        assert_eq!(conversation.as_ref().unwrap().channel, "telegram");
        assert_eq!(conversation.as_ref().unwrap().chat_id, "c1");
    }

    #[test]
    fn sqlite_store_rejects_duplicate_names() {
        let store = temp_store();
        let automation = send_message_automation("daily-news", Trigger::cron("0 9 * * *").unwrap());

        store.insert(&automation).unwrap();
        let duplicate = store.insert(&automation);

        assert!(duplicate.is_err());
    }

    #[test]
    fn sqlite_store_deletes_automations() {
        let store = temp_store();
        let automation = send_message_automation(
            "later",
            Trigger::at(&chrono::Local::now().to_rfc3339()).unwrap(),
        );

        store.insert(&automation).unwrap();
        assert_eq!(store.load_all().unwrap().len(), 1);

        store.delete("later").unwrap();

        assert!(store.load_all().unwrap().is_empty());
    }
}
