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

    pub async fn execute(&self, input: SendMessageToolInput) -> Result<SendMessageOutput, String> {
        let attachments = input
            .file_id
            .map(|file_id| (self.attachment_resolver)(file_id))
            .transpose()?
            .into_iter()
            .collect();
        let channel = input.channel.unwrap_or_default();
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
        Ok(SendMessageOutput { ok: true, channel })
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
        let output = SendMessageTool::execute(self, input).await?;
        Ok(tools::ToolOutput::json(
            serde_json::to_value(output).map_err(|err| err.to_string())?,
        ))
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
    })
}

#[derive(serde::Serialize)]
pub struct SendMessageOutput {
    ok: bool,
    channel: String,
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
}
