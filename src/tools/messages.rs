use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    bus::{Bus, ChannelAddress, Event, Sender},
    message::{Attachment, Message},
    tools::{self, InternalTool, ToolContext},
};

type AttachmentResolver = Arc<dyn Fn(u32) -> Result<Attachment, String> + Send + Sync>;

#[derive(Clone)]
pub struct SendMessageTool {
    bus: Bus,
    attachment_resolver: AttachmentResolver,
}

impl SendMessageTool {
    pub fn new(
        bus: Bus,
        attachment_resolver: impl Fn(u32) -> Result<Attachment, String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            bus,
            attachment_resolver: Arc::new(attachment_resolver),
        }
    }

    pub async fn execute(&self, input: SendMessageToolInput) -> Result<serde_json::Value, String> {
        let attachments = input
            .file_id
            .map(|file_id| (self.attachment_resolver)(file_id))
            .transpose()?
            .into_iter()
            .collect();
        let channel = input
            .channel
            .filter(|channel| !channel.trim().is_empty())
            .ok_or_else(|| "channel is required".to_string())?;
        let address = message_address(&channel, input.conversation_id, input.thread_id);
        let message = Message::Assistant {
            text: Some(input.content.unwrap_or_default()),
            attachments,
            tool_calls: Vec::new(),
        };
        self.bus.publish(Event::MessageOut {
            channel: channel.clone(),
            recipient: Sender::User,
            address,
            message,
        });
        Ok(serde_json::json!({ "ok": true, "channel": channel }))
    }
}

#[async_trait]
impl InternalTool for SendMessageTool {
    fn name(&self) -> &str {
        "send_message"
    }

    fn tool_definition(&self) -> crate::message::ToolDefinition {
        tools::tool_definition::<SendMessageToolInput>(
            self.name(),
            "Send a message to the user through one of Warden's channels. \
             Use this only to reach a different channel or conversation than \
             the current one, or to deliver a file: your final reply is \
             delivered to the current conversation automatically.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<tools::ToolOutput, String> {
        let mut input: SendMessageToolInput =
            serde_json::from_value(args).map_err(|err| err.to_string())?;
        input.fill_current_destination(ctx);
        SendMessageTool::execute(self, input).await
    }
}

#[derive(schemars::JsonSchema, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendMessageToolInput {
    #[serde(default)]
    #[schemars(
        description = "Optional channel to send through, for example telegram or tui. If omitted, Warden replies in the current conversation. If provided without conversation_id, that channel's default destination is used."
    )]
    pub channel: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "ID of a generated file to attach. Use this alone when delivering an image from image_generation."
    )]
    pub file_id: Option<u32>,
    #[serde(default)]
    #[schemars(
        description = "Optional message text to send. Omit this when sending only a generated file."
    )]
    pub content: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Optional channel conversation id. If omitted for an explicit channel, Warden uses that channel's default destination."
    )]
    pub conversation_id: Option<String>,
    #[serde(default)]
    #[schemars(description = "Optional channel thread/topic id.")]
    pub thread_id: Option<String>,
}

impl SendMessageToolInput {
    /// When the model names no channel, reply in the conversation the
    /// current run was triggered from.
    fn fill_current_destination(&mut self, ctx: &ToolContext) {
        if self.channel.is_some() {
            return;
        }
        let Some((channel, address)) = &ctx.destination else {
            return;
        };
        self.channel = Some(channel.clone());
        if self.conversation_id.is_none() {
            self.conversation_id = Some(address.conversation_id.clone());
        }
        if self.thread_id.is_none() {
            self.thread_id = address.thread_id.clone();
        }
    }
}

fn message_address(
    channel: &str,
    conversation_id: Option<String>,
    thread_id: Option<String>,
) -> Option<ChannelAddress> {
    conversation_id.map(|conversation_id| ChannelAddress {
        channel: channel.to_string(),
        conversation_id,
        thread_id,
        message_id: None,
    })
}

#[derive(Clone)]
pub struct DeleteMessageTool {
    bus: Bus,
}

impl DeleteMessageTool {
    pub fn new(bus: Bus) -> Self {
        Self { bus }
    }

    pub async fn execute(
        &self,
        input: DeleteMessageToolInput,
    ) -> Result<serde_json::Value, String> {
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

        self.bus.publish(Event::DeleteMessage {
            channel: channel.clone(),
            address: ChannelAddress {
                channel,
                conversation_id,
                thread_id: input.thread_id,
                message_id: Some(message_id),
            },
        });
        Ok(serde_json::json!({ "ok": true }))
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
        DeleteMessageTool::execute(self, input).await
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

#[cfg(test)]
mod tests {
    use super::*;

    fn input(args: serde_json::Value) -> SendMessageToolInput {
        serde_json::from_value(args).expect("valid input")
    }

    fn tui_context() -> ToolContext {
        ToolContext {
            destination: Some((
                "telegram".to_string(),
                ChannelAddress {
                    channel: "telegram".to_string(),
                    conversation_id: "123".to_string(),
                    thread_id: Some("456".to_string()),
                    message_id: None,
                },
            )),
            conversation_id: Some("123".to_string()),
            ..ToolContext::default()
        }
    }

    #[test]
    fn explicit_channel_is_left_untouched() {
        let mut input = input(serde_json::json!({
            "channel": "telegram",
            "content": "hi"
        }));

        input.fill_current_destination(&ToolContext {
            destination: Some((
                "tui".to_string(),
                ChannelAddress {
                    channel: "tui".to_string(),
                    conversation_id: "http://127.0.0.1:60684".to_string(),
                    thread_id: None,
                    message_id: None,
                },
            )),
            conversation_id: None,
            ..ToolContext::default()
        });

        assert_eq!(input.channel.as_deref(), Some("telegram"));
        assert!(input.conversation_id.is_none());
        assert!(input.thread_id.is_none());
    }

    #[test]
    fn omitted_channel_defaults_to_current_conversation() {
        let mut input = input(serde_json::json!({ "content": "hi" }));

        input.fill_current_destination(&tui_context());

        assert_eq!(input.channel.as_deref(), Some("telegram"));
        assert_eq!(input.conversation_id.as_deref(), Some("123"));
        assert_eq!(input.thread_id.as_deref(), Some("456"));
    }

    #[test]
    fn detached_context_fills_nothing() {
        let mut input = input(serde_json::json!({ "content": "hi" }));

        input.fill_current_destination(&ToolContext::default());

        assert!(input.channel.is_none());
        assert!(input.conversation_id.is_none());
    }

    #[tokio::test]
    async fn send_message_requires_channel_after_context_defaults() {
        let bus = Bus::new(16);
        let tool = SendMessageTool::new(bus, |_| Err("unexpected attachment".to_string()));

        let result = tool
            .execute(SendMessageToolInput {
                channel: None,
                file_id: None,
                content: Some("hi".to_string()),
                conversation_id: None,
                thread_id: None,
            })
            .await;

        assert_eq!(result.err().as_deref(), Some("channel is required"));
    }

    #[tokio::test]
    async fn send_message_publishes_channel_delivery_request_when_address_is_known() {
        let bus = Bus::new(16);
        let mut events = bus.subscribe();
        let tool = SendMessageTool::new(bus, |_| Err("unexpected attachment".to_string()));

        tool.execute(SendMessageToolInput {
            channel: Some("telegram".to_string()),
            file_id: None,
            content: Some("hi".to_string()),
            conversation_id: Some("chat-1".to_string()),
            thread_id: Some("thread-1".to_string()),
        })
        .await
        .unwrap();

        let Event::MessageOut {
            channel,
            address,
            message,
            ..
        } = events.try_recv().unwrap()
        else {
            panic!("expected message out");
        };
        assert_eq!(channel, "telegram");
        let address = address.expect("address");
        assert_eq!(address.conversation_id, "chat-1");
        assert_eq!(address.thread_id.as_deref(), Some("thread-1"));
        let Message::Assistant { text, .. } = message else {
            panic!("expected assistant message");
        };
        assert_eq!(text.as_deref(), Some("hi"));
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
    async fn delete_message_publishes_channel_delete_request() {
        let bus = Bus::new(16);
        let mut events = bus.subscribe();
        let tool = DeleteMessageTool::new(bus);

        tool.execute(DeleteMessageToolInput {
            channel: Some("telegram".to_string()),
            conversation_id: Some("chat-1".to_string()),
            message_id: Some("provider-1".to_string()),
            thread_id: None,
        })
        .await
        .unwrap();

        let Event::DeleteMessage { channel, address } = events.try_recv().unwrap() else {
            panic!("expected delete message event");
        };
        assert_eq!(channel, "telegram");
        assert_eq!(address.conversation_id, "chat-1");
        assert_eq!(address.message_id.as_deref(), Some("provider-1"));
    }
}
