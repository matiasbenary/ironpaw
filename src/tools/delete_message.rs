use std::sync::{Arc, OnceLock};

use async_trait::async_trait;

use crate::{
    bus::{Bus, ChannelAddress, Event},
    history::History,
    tools::{self, InternalTool, ToolContext},
};

#[derive(Clone)]
pub struct DeleteMessageTool {
    bus: Bus,
    history: OnceLock<Arc<dyn History>>,
}

impl DeleteMessageTool {
    pub fn new(bus: Bus) -> Self {
        Self {
            bus,
            history: OnceLock::new(),
        }
    }

    pub fn bind_history(&self, history: Arc<dyn History>) {
        let _ = self.history.set(history);
    }

    pub async fn execute(
        &self,
        input: DeleteMessageToolInput,
    ) -> Result<DeleteMessageOutput, String> {
        let channel = input
            .channel
            .filter(|channel| !channel.trim().is_empty())
            .ok_or_else(|| "channel is required".to_string())?;
        let conversation_id = input
            .conversation_id
            .filter(|conversation_id| !conversation_id.trim().is_empty())
            .ok_or_else(|| "conversation_id is required".to_string())?;
        let message_id = input
            .message_id
            .filter(|message_id| !message_id.trim().is_empty())
            .ok_or_else(|| "message_id is required".to_string())?;

        if let Some(history) = self.history.get() {
            history.delete_external_message(
                &channel,
                &conversation_id,
                input.thread_id.as_deref(),
                &message_id,
            );
        }
        self.bus.publish(Event::DeleteMessage {
            channel: channel.clone(),
            address: ChannelAddress {
                channel,
                conversation_id,
                thread_id: input.thread_id,
                message_id: Some(message_id),
            },
        });
        Ok(DeleteMessageOutput { ok: true })
    }
}

#[async_trait]
impl InternalTool for DeleteMessageTool {
    fn name(&self) -> &str {
        "delete_message"
    }

    fn tool_definition(&self) -> crate::message::ToolDefinition {
        tools::tool_definition::<DeleteMessageToolInput>(
            self.name(),
            "Delete a message from a channel when the channel supports it. \
             If called while handling an inbound message, omit arguments to \
             delete that triggering message.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<tools::ToolOutput, String> {
        let mut input: DeleteMessageToolInput =
            serde_json::from_value(args).map_err(|err| err.to_string())?;
        input.fill_current_message(ctx);
        let output = DeleteMessageTool::execute(self, input).await?;
        Ok(tools::ToolOutput::json(
            serde_json::to_value(output).map_err(|err| err.to_string())?,
        ))
    }
}

#[derive(schemars::JsonSchema, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteMessageToolInput {
    #[serde(default)]
    #[schemars(
        description = "Optional channel. If omitted, uses the current inbound message channel."
    )]
    pub channel: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Optional channel conversation id. If omitted, uses the current inbound message conversation."
    )]
    pub conversation_id: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Optional message id. If omitted, uses the current inbound message id."
    )]
    pub message_id: Option<String>,
    #[serde(default)]
    #[schemars(description = "Optional channel thread/topic id.")]
    pub thread_id: Option<String>,
}

impl DeleteMessageToolInput {
    fn fill_current_message(&mut self, ctx: &ToolContext) {
        let Some((channel, address)) = &ctx.destination else {
            return;
        };
        if self.channel.is_none() {
            self.channel = Some(channel.clone());
        }
        if self.conversation_id.is_none() {
            self.conversation_id = Some(address.conversation_id.clone());
        }
        if self.message_id.is_none() {
            self.message_id = address.message_id.clone();
        }
        if self.thread_id.is_none() {
            self.thread_id = address.thread_id.clone();
        }
    }
}

#[derive(serde::Serialize)]
pub struct DeleteMessageOutput {
    ok: bool,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{
        bus::{ChannelAddress, Event},
        history::{History, HistoryEntry, MessageMetadata, SqliteHistory},
        message::Message,
    };

    fn temp_history() -> Arc<SqliteHistory> {
        let path = std::env::temp_dir().join(format!(
            "warden-delete-message-tool-history-{}.db",
            uuid::Uuid::new_v4()
        ));
        Arc::new(SqliteHistory::new(path).unwrap())
    }

    #[tokio::test]
    async fn delete_message_defaults_to_current_message() {
        let bus = Bus::new(16);
        let mut events = bus.subscribe();
        let tool = DeleteMessageTool::new(bus);
        let ctx = ToolContext {
            destination: Some((
                "telegram".to_string(),
                ChannelAddress {
                    channel: "telegram".to_string(),
                    conversation_id: "123".to_string(),
                    thread_id: None,
                    message_id: Some("456".to_string()),
                },
            )),
            ..ToolContext::default()
        };

        <DeleteMessageTool as InternalTool>::execute(&tool, serde_json::json!({}), &ctx)
            .await
            .unwrap();

        let Event::DeleteMessage { channel, address } = events.try_recv().unwrap() else {
            panic!("expected delete message event");
        };
        assert_eq!(channel, "telegram");
        assert_eq!(address.conversation_id, "123");
        assert_eq!(address.message_id.as_deref(), Some("456"));
    }

    #[tokio::test]
    async fn delete_message_removes_matching_channel_history() {
        let bus = Bus::new(16);
        let history = temp_history();
        history.append(
            "chat-1",
            &[HistoryEntry {
                message: Message::User {
                    text: "secret".to_string(),
                    attachments: Vec::new(),
                },
                metadata: MessageMetadata::new(
                    "telegram",
                    "chat-1",
                    None,
                    Some("provider-1".to_string()),
                ),
            }],
        );
        let tool = DeleteMessageTool::new(bus);
        tool.bind_history(history.clone());

        tool.execute(DeleteMessageToolInput {
            channel: Some("telegram".to_string()),
            conversation_id: Some("chat-1".to_string()),
            message_id: Some("provider-1".to_string()),
            thread_id: None,
        })
        .await
        .unwrap();

        assert!(history.load("chat-1").is_empty());
    }
}
