use std::path::PathBuf;

use async_trait::async_trait;

use crate::{
    channels::local_client::{EVENT_TIME, ReminderEvent, device_events_path, read_events},
    tools::{self, ExecutionContext, InternalTool, ToolError, ToolResult},
};

/// Reads or replaces the reminders the ESP32 rings, from any conversation (the board's own
/// conversation can also edit `events.json` with the file tools; it is the same file).
pub struct DeviceRemindersTool {
    files_dir: PathBuf,
}

impl DeviceRemindersTool {
    pub fn new(files_dir: PathBuf) -> Self {
        Self { files_dir }
    }
}

#[derive(schemars::JsonSchema, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DeviceRemindersInput {
    /// The complete new list. Omit it to only read the current one.
    events: Option<Vec<ReminderEvent>>,
}

#[async_trait]
impl InternalTool for DeviceRemindersTool {
    tools::tool_metadata!(
        "device_reminders",
        DeviceRemindersInput,
        "Read or replace the reminders the user's voice device (the ESP32 board) rings on \
        its own. Use it when the user asks to schedule a meeting or be reminded of something. \
        Without `events` it returns the current list. With `events` it replaces the whole \
        list, so read first and send every reminder you keep plus the new or changed one. \
        Each reminder: `at` is local time `YYYY-MM-DDTHH:MM`, `text` is what the board shows, \
        `repeat` is optional `daily` or `weekly`, `sound` an optional WAV on its SD card. \
        The board picks up changes within 10 minutes."
    );

    async fn execute(&self, args: serde_json::Value, _ctx: &ExecutionContext) -> ToolResult {
        let input: DeviceRemindersInput =
            serde_json::from_value(args).map_err(|e| ToolError::new(e.to_string()))?;
        let path = device_events_path(&self.files_dir);
        if let Some(events) = input.events {
            for event in &events {
                chrono::NaiveDateTime::parse_from_str(&event.at, EVENT_TIME).map_err(|_| {
                    ToolError::new(format!("{:?} is not YYYY-MM-DDTHH:MM", event.at))
                })?;
            }
            let text = serde_json::to_string_pretty(&events).expect("reminders serialize");
            if let Some(dir) = path.parent() {
                tokio::fs::create_dir_all(dir)
                    .await
                    .map_err(|e| ToolError::new(e.to_string()))?;
            }
            tokio::fs::write(&path, text)
                .await
                .map_err(|e| ToolError::new(e.to_string()))?;
        }
        let events = match tokio::fs::read_to_string(&path).await {
            Ok(text) => read_events(&text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(ToolError::new(error.to_string())),
        };
        Ok(serde_json::json!({ "events": events }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn replaces_and_reads_the_board_reminders() {
        let dir = std::env::temp_dir().join(format!("ironpaw-reminders-{}", uuid::Uuid::new_v4()));
        let tool = DeviceRemindersTool::new(dir.clone());
        let ctx = ExecutionContext::new(PathBuf::from("conversations/webui"));
        let meeting = serde_json::json!({ "at": "2026-11-05T11:00", "text": "Meeting" });

        let written = tool
            .execute(serde_json::json!({ "events": [meeting] }), &ctx)
            .await
            .unwrap();
        let read = tool.execute(serde_json::json!({}), &ctx).await.unwrap();

        assert_eq!(written, read);
        assert_eq!(read["events"][0]["text"], "Meeting");
        assert!(device_events_path(&dir).exists());
        let bad = serde_json::json!({ "events": [{ "at": "tomorrow", "text": "x" }] });
        assert!(tool.execute(bad, &ctx).await.is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
