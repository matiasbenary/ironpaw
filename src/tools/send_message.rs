use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    bus::{Bus, ChannelAddress, Event, Sender},
    message::{Attachment, Message},
    tools::{self, InternalTool},
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
    fn name(&self) -> &'static str {
        "send_message"
    }

    fn tool_definition(&self) -> crate::message::ToolDefinition {
        tools::tool_definition::<SendMessageToolInput>(
            self.name(),
            "Send a message to the user through one of Warden's channels.",
        )
    }

    async fn execute(&self, args: serde_json::Value) -> Result<tools::ToolOutput, String> {
        let input: SendMessageToolInput =
            serde_json::from_value(args).map_err(|err| err.to_string())?;
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
