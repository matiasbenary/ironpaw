use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    bus::{Bus, ConversationKey, Event, InvocationScope, MessageRef},
    message::{Attachment, Message},
    tools::{self, ExecutionContext, InternalTool, ToolResponder},
};

type AttachmentResolver = Arc<dyn Fn(&str) -> Result<Attachment, String> + Send + Sync>;

#[derive(Clone)]
pub struct SendMessageTool {
    bus: Bus,
    attachment_resolver: AttachmentResolver,
}

impl SendMessageTool {
    pub fn new(
        bus: Bus,
        attachment_resolver: impl Fn(&str) -> Result<Attachment, String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            bus,
            attachment_resolver: Arc::new(attachment_resolver),
        }
    }

    pub fn execute(&self, input: SendMessageToolInput) -> Result<serde_json::Value, String> {
        let attachments = input
            .filename
            .as_deref()
            .map(|filename| (self.attachment_resolver)(filename))
            .transpose()?
            .into_iter()
            .collect();
        if input.channel.trim().is_empty() {
            return Err("channel must not be empty".to_string());
        }
        let message = Message::Assistant {
            text: Some(input.content.unwrap_or_default()),
            attachments,
            tool_calls: Vec::new(),
        };
        self.bus.publish(Event::MessageOut {
            channel: input.channel.clone(),
            chat_id: input.chat_id.clone(),
            thread_id: input.thread_id,
            message_id: input.message_id.clone(),
            message,
        });
        Ok(serde_json::json!({
            "ok": true,
            "channel": input.channel,
            "chat_id": input.chat_id,
            "message_id": input.message_id,
        }))
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
            "Send a message to the user through one of Ironpaw's channels. \
             Use this only to reach a different channel or conversation than \
             the current one, or to deliver a file: your final reply is \
             delivered to the current conversation automatically.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ExecutionContext,
        responder: ToolResponder,
    ) {
        let result = serde_json::from_value(args)
            .map_err(|err| err.to_string())
            .and_then(|input| SendMessageTool::execute(self, input))
            .map_err(crate::tools::ToolError::new);
        responder.respond(result);
    }
}

#[derive(schemars::JsonSchema, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendMessageToolInput {
    /// Channel to send through, for example telegram or tui.
    pub channel: String,
    #[serde(default)]
    #[schemars(description = "Filename to attach. Use the filename returned by image_generation.")]
    pub filename: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Optional message text to send. Omit this when sending only a generated file."
    )]
    pub content: Option<String>,
    /// Optional provider message id to use for this outbound message.
    #[serde(default)]
    pub message_id: Option<String>,
    /// Optional destination chat id within the channel. When omitted, the
    /// channel's first paired conversation is used.
    #[serde(default)]
    pub chat_id: Option<String>,
    #[serde(default)]
    #[schemars(description = "Optional channel thread/topic id.")]
    pub thread_id: Option<String>,
}

#[derive(Clone)]
pub struct DeleteMessageTool {
    bus: Bus,
}

impl DeleteMessageTool {
    pub fn new(bus: Bus) -> Self {
        Self { bus }
    }

    pub fn execute(&self, input: DeleteMessageToolInput) -> Result<serde_json::Value, String> {
        let channel = input
            .channel
            .filter(|channel| !channel.trim().is_empty())
            .ok_or_else(|| "channel is required".to_string())?;
        let chat_id = input
            .chat_id
            .filter(|chat_id| !chat_id.trim().is_empty())
            .ok_or_else(|| "chat_id is required".to_string())?;
        let message_id = input
            .message_id
            .filter(|message_id| !message_id.trim().is_empty())
            .ok_or_else(|| "message_id is required".to_string())?;

        self.bus.publish(Event::DeleteMessage {
            message: MessageRef {
                conversation: ConversationKey {
                    channel,
                    chat_id,
                    thread_id: input.thread_id,
                },
                message_id,
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
             Defaults to the current conversation only when called from a \
             conversation-scoped execution.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ExecutionContext,
        responder: ToolResponder,
    ) {
        let result = serde_json::from_value(args)
            .map_err(|err| err.to_string())
            .and_then(|mut input: DeleteMessageToolInput| {
                input.fill_current_message(ctx);
                DeleteMessageTool::execute(self, input)
            })
            .map_err(crate::tools::ToolError::new);
        responder.respond(result);
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
        description = "Optional chat id. If omitted, uses the current inbound message conversation."
    )]
    pub chat_id: Option<String>,
    #[serde(default)]
    #[schemars(description = "Provider message id to delete.")]
    pub message_id: Option<String>,
    #[serde(default)]
    #[schemars(description = "Optional channel thread/topic id.")]
    pub thread_id: Option<String>,
}

impl DeleteMessageToolInput {
    fn fill_current_message(&mut self, ctx: &ExecutionContext) {
        let conversation = match &ctx.invoked_by {
            InvocationScope::Conversation(conversation) => conversation,
            InvocationScope::ScheduledAutomation { .. } => return,
        };
        if self.channel.is_none() {
            self.channel = Some(conversation.channel.clone());
        }
        if self.chat_id.is_none() {
            self.chat_id = Some(conversation.chat_id.clone());
        }
        if self.thread_id.is_none() {
            self.thread_id = conversation.thread_id.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_message_requires_channel_but_not_chat_id() {
        let definition =
            SendMessageTool::new(Bus::new(1), |_| Err("unused".to_string())).tool_definition();
        let required = definition.parameters["required"].as_array().unwrap();

        assert!(required.contains(&serde_json::json!("channel")));
        assert!(!required.contains(&serde_json::json!("chat_id")));
        assert!(
            serde_json::from_value::<SendMessageToolInput>(serde_json::json!({
                "channel": "telegram",
                "content": "hi"
            }))
            .is_ok()
        );
    }

    #[test]
    fn send_message_publishes_channel_delivery_request_when_conversation_is_known() {
        let bus = Bus::new(16);
        let mut events = bus.subscribe();
        let tool = SendMessageTool::new(bus, |_| Err("unexpected attachment".to_string()));

        tool.execute(SendMessageToolInput {
            channel: "telegram".to_string(),
            filename: None,
            content: Some("hi".to_string()),
            chat_id: Some("chat-1".to_string()),
            thread_id: Some("thread-1".to_string()),
            message_id: None,
        })
        .unwrap();

        let Event::MessageOut {
            channel,
            chat_id,
            thread_id,
            message,
            ..
        } = events.try_recv().unwrap()
        else {
            panic!("expected message out");
        };
        assert_eq!(channel, "telegram");
        assert_eq!(chat_id.as_deref(), Some("chat-1"));
        assert_eq!(thread_id.as_deref(), Some("thread-1"));
        let Message::Assistant { text, .. } = message else {
            panic!("expected assistant message");
        };
        assert_eq!(text.as_deref(), Some("hi"));
    }

    #[tokio::test]
    async fn delete_message_defaults_conversation_but_requires_message_id() {
        let bus = Bus::new(16);
        let mut events = bus.subscribe();
        let tool = Arc::new(DeleteMessageTool::new(bus));
        let ctx = ExecutionContext::new(InvocationScope::Conversation(ConversationKey {
            channel: "telegram".to_string(),
            chat_id: "123".to_string(),
            thread_id: None,
        }));

        let _ =
            tools::run_internal_tool(tool, serde_json::json!({ "message_id": "456" }), &ctx).await;

        let Event::DeleteMessage { message } = events.try_recv().unwrap() else {
            panic!("expected delete message event");
        };
        assert_eq!(message.conversation.channel, "telegram");
        assert_eq!(message.conversation.chat_id, "123");
        assert_eq!(message.message_id, "456");
    }

    #[test]
    fn delete_message_publishes_channel_delete_request() {
        let bus = Bus::new(16);
        let mut events = bus.subscribe();
        let tool = DeleteMessageTool::new(bus);

        tool.execute(DeleteMessageToolInput {
            channel: Some("telegram".to_string()),
            chat_id: Some("chat-1".to_string()),
            message_id: Some("provider-1".to_string()),
            thread_id: None,
        })
        .unwrap();

        let Event::DeleteMessage { message } = events.try_recv().unwrap() else {
            panic!("expected delete message event");
        };
        assert_eq!(message.conversation.channel, "telegram");
        assert_eq!(message.conversation.chat_id, "chat-1");
        assert_eq!(message.message_id, "provider-1");
    }
}
