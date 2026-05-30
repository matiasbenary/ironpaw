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

    pub async fn execute(&self, params: SendMessageParams) -> Result<SendMessageOutput, String> {
        let mut message = params.message();
        if let Some(file_id) = params.file_id {
            let path = self.files_dir.join(format!("{file_id}.png"));
            let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
            use base64::Engine as _;
            let data_base64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            message.attachments.push(OutboundAttachment {
                kind: "image".to_string(),
                mime_type: "image/png".to_string(),
                filename: format!("{file_id}.png"),
                data_base64: Some(data_base64),
                url: None,
            });
        }
        self.bus.publish(Event::MessageOut {
            channel: params.channel.clone(),
            recipient: Sender::User,
            address: params.address(),
            message,
        });
        Ok(SendMessageOutput {
            ok: true,
            channel: params.channel,
        })
    }
}

#[async_trait]
impl InternalTool for SendMessageTool {
    fn name(&self) -> &'static str {
        "send_message"
    }

    fn responses_schema(&self) -> crate::openai::Tool {
        tools::responses_tool_schema::<SendMessageToolInput>(
            self.name(),
            "Send a message to the user through one of Warden's channels.",
        )
    }

    async fn execute(&self, args: serde_json::Value) -> Result<tools::ToolOutput, String> {
        let params: SendMessageParams =
            serde_json::from_value(args).map_err(|err| err.to_string())?;
        let output = SendMessageTool::execute(self, params).await?;
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
    #[schemars(description = "The message text to send.")]
    pub content: String,
    #[serde(default)]
    #[schemars(
        description = "ID of a generated file to attach. Use the file_id returned by image_generation."
    )]
    pub file_id: Option<u32>,
    #[serde(default)]
    #[schemars(description = "Optional public image URL to send with the message.")]
    pub image_url: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Optional base64-encoded generated image data to send with the message."
    )]
    pub image_base64: Option<String>,
    #[serde(default)]
    #[schemars(description = "MIME type for image_base64. Defaults to image/png.")]
    pub image_mime_type: Option<String>,
    #[serde(default)]
    #[schemars(description = "Filename for image_base64. Defaults to generated-image.png.")]
    pub image_filename: Option<String>,
    #[serde(default)]
    #[schemars(
        description = "Optional channel conversation id. If omitted for an explicit channel, Warden uses that channel's default destination."
    )]
    pub conversation_id: Option<String>,
    #[serde(default)]
    #[schemars(description = "Optional channel thread/topic id.")]
    pub thread_id: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendMessageParams {
    pub channel: String,
    pub content: String,
    #[serde(default)]
    pub file_id: Option<u32>,
    #[serde(default)]
    pub image_url: Option<String>,
    #[serde(default)]
    pub image_base64: Option<String>,
    #[serde(default)]
    pub image_mime_type: Option<String>,
    #[serde(default)]
    pub image_filename: Option<String>,
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
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

    fn message(&self) -> OutboundMessage {
        let mut message = OutboundMessage {
            content: Some(self.content.clone()),
            attachments: Vec::new(),
        };
        if let Some(image_base64) = &self.image_base64 {
            message.attachments.push(OutboundAttachment {
                kind: "image".to_string(),
                mime_type: non_empty_or_default(&self.image_mime_type, "image/png"),
                filename: non_empty_or_default(&self.image_filename, "generated-image.png"),
                data_base64: Some(image_base64.clone()),
                url: None,
            });
        } else if let Some(image_url) = &self.image_url {
            message.attachments.push(OutboundAttachment {
                kind: "image".to_string(),
                mime_type: non_empty_or_default(&self.image_mime_type, "image/png"),
                filename: non_empty_or_default(&self.image_filename, "generated-image.png"),
                data_base64: None,
                url: Some(image_url.clone()),
            });
        }
        message
    }
}

fn non_empty_or_default(value: &Option<String>, default: &str) -> String {
    value
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(default)
        .to_string()
}

#[derive(serde::Serialize)]
pub struct SendMessageOutput {
    ok: bool,
    channel: String,
}
