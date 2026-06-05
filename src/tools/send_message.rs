use std::path::PathBuf;

use async_trait::async_trait;

use crate::{
    bus::{Bus, ChannelAddress, Event, Sender},
    message::{OutboundAttachment, OutboundMessage},
    tools::{self, InternalTool},
};

#[derive(Clone)]
pub struct SendMessageTool {
    bus: Bus,
    files_dir: PathBuf,
}

impl SendMessageTool {
    pub fn new(bus: Bus, files_dir: PathBuf) -> Self {
        Self { bus, files_dir }
    }

    pub async fn execute(
        &self,
        mut params: SendMessageParams,
    ) -> Result<SendMessageOutput, String> {
        for attachment in &mut params.attachments {
            if let Some(file_id) = attachment.file_id {
                if attachment.data_base64.is_none() {
                    let path = self.files_dir.join(format!("{file_id}.png"));
                    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
                    use base64::Engine as _;
                    attachment.data_base64 =
                        Some(base64::engine::general_purpose::STANDARD.encode(&bytes));
                }
            }
        }
        let channel = params.channel.clone();
        let address = params.address();
        let message = OutboundMessage {
            content: Some(params.content),
            attachments: params.attachments,
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
        let output = SendMessageTool::execute(self, SendMessageParams::from(input)).await?;
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
    #[schemars(description = "Optional message text to send. Omit this when sending only a generated file.")]
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

pub struct SendMessageParams {
    pub channel: String,
    pub content: String,
    pub attachments: Vec<OutboundAttachment>,
    pub conversation_id: Option<String>,
    pub thread_id: Option<String>,
}

impl SendMessageParams {
    fn address(&self) -> Option<ChannelAddress> {
        self.conversation_id
            .as_ref()
            .map(|conversation_id| ChannelAddress {
                channel: self.channel.clone(),
                conversation_id: conversation_id.clone(),
                thread_id: self.thread_id.clone(),
            })
    }
}

impl From<SendMessageToolInput> for SendMessageParams {
    fn from(input: SendMessageToolInput) -> Self {
        let attachments = input.file_id.map(|file_id| OutboundAttachment {
            kind: "image".to_string(),
            mime_type: "image/png".to_string(),
            filename: format!("{file_id}.png"),
            file_id: Some(file_id),
            data_base64: None,
            url: None,
        });
        Self {
            channel: input.channel.unwrap_or_default(),
            content: input.content.unwrap_or_default(),
            attachments: attachments.into_iter().collect(),
            conversation_id: input.conversation_id,
            thread_id: input.thread_id,
        }
    }
}

#[derive(serde::Serialize)]
pub struct SendMessageOutput {
    ok: bool,
    channel: String,
}
