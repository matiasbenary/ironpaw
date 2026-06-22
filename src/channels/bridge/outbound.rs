use std::{path::PathBuf, sync::Arc};

use serde::Serialize;

use crate::{
    bus::{Bus, ChannelAction, ChannelAddress, Event, Sender},
    channels::bridge::{ChannelRegistry, LoadedChannel, history::append_channel_history},
    history::History,
    message::{Attachment, Message, ToolCall},
};

pub(super) async fn message_warden_to_channel(
    bus: Bus,
    history: Arc<dyn History>,
    registry: Arc<ChannelRegistry>,
) {
    let mut events = bus.subscribe();
    loop {
        let (channel_runtime, channel, event) = match events.recv().await {
            Ok(Event::MessageOut {
                channel,
                recipient: Sender::User,
                address,
                message,
            }) => {
                let Some(channel_runtime) = registry.by_name(&channel).await else {
                    eprintln!("bridge: no loaded channel named {channel} for message out");
                    continue;
                };
                let channel_state = channel_runtime.state.lock().await.clone();
                let Some(address) = address.or(channel_state.config.default_address) else {
                    eprintln!("{channel} message out has no address and no default_address");
                    continue;
                };
                if !matches!(message, Message::Assistant { .. }) {
                    eprintln!("{channel} message out was not deliverable to a channel");
                    continue;
                }
                append_channel_history(history.as_ref(), &channel, &address, None, &message);
                (
                    channel_runtime,
                    channel.clone(),
                    Event::MessageOut {
                        channel,
                        recipient: Sender::User,
                        address: Some(address),
                        message,
                    },
                )
            }
            Ok(Event::DeleteMessage { channel, address }) => {
                let Some(channel_runtime) = registry.by_name(&channel).await else {
                    eprintln!("bridge: no loaded channel named {channel} for delete message");
                    continue;
                };
                let Some(message_id) = address.message_id.as_deref() else {
                    eprintln!("{channel} delete message has no message_id");
                    continue;
                };
                history.delete_external_message(
                    &channel,
                    &address.conversation_id,
                    address.thread_id.as_deref(),
                    message_id,
                );
                (
                    channel_runtime,
                    channel.clone(),
                    Event::DeleteMessage { channel, address },
                )
            }
            Ok(Event::ChannelAction {
                channel,
                recipient: Sender::User,
                address,
                action,
            }) => {
                let Some(channel_runtime) = registry.by_name(&channel).await else {
                    eprintln!("bridge: no loaded channel named {channel} for channel action");
                    continue;
                };
                (
                    channel_runtime,
                    channel.clone(),
                    Event::ChannelAction {
                        channel,
                        recipient: Sender::User,
                        address,
                        action,
                    },
                )
            }
            Ok(_) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        };
        let channel_state = channel_runtime.state.lock().await.clone();
        dispatch_channel_event(
            &channel_runtime,
            &channel,
            &channel_state.config_json,
            &event,
        )
        .await;
    }
}

#[derive(Serialize)]
enum ChannelOutboundEvent {
    MessageOut {
        channel: String,
        recipient: Sender,
        address: Option<ChannelAddress>,
        message: ChannelOutboundMessage,
    },
    DeleteMessage {
        channel: String,
        address: ChannelAddress,
    },
    ChannelAction {
        channel: String,
        recipient: Sender,
        address: ChannelAddress,
        action: ChannelAction,
    },
}

impl TryFrom<&Event> for ChannelOutboundEvent {
    type Error = String;

    fn try_from(event: &Event) -> Result<Self, Self::Error> {
        match event {
            Event::MessageOut {
                channel,
                recipient,
                address,
                message,
            } => Ok(Self::MessageOut {
                channel: channel.clone(),
                recipient: recipient.clone(),
                address: address.clone(),
                message: ChannelOutboundMessage::try_from(message)?,
            }),
            Event::DeleteMessage { channel, address } => Ok(Self::DeleteMessage {
                channel: channel.clone(),
                address: address.clone(),
            }),
            Event::ChannelAction {
                channel,
                recipient,
                address,
                action,
            } => Ok(Self::ChannelAction {
                channel: channel.clone(),
                recipient: recipient.clone(),
                address: address.clone(),
                action: action.clone(),
            }),
            _ => Err("event is not deliverable to a channel".to_string()),
        }
    }
}

#[derive(Serialize)]
enum ChannelOutboundMessage {
    Assistant {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<ChannelOutboundAttachment>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
}

impl TryFrom<&Message> for ChannelOutboundMessage {
    type Error = String;

    fn try_from(message: &Message) -> Result<Self, Self::Error> {
        let Message::Assistant {
            text,
            attachments,
            tool_calls,
        } = message
        else {
            return Err("message is not deliverable to a channel".to_string());
        };
        Ok(Self::Assistant {
            text: text.clone(),
            attachments: attachments
                .iter()
                .map(ChannelOutboundAttachment::from)
                .collect(),
            tool_calls: tool_calls.clone(),
        })
    }
}

#[derive(Serialize)]
struct ChannelOutboundAttachment {
    file_id: u32,
    mime_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    filename: Option<String>,
    source: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    data_base64: Option<String>,
}

impl From<&Attachment> for ChannelOutboundAttachment {
    fn from(attachment: &Attachment) -> Self {
        let filename = attachment.filename.clone().or_else(|| {
            attachment
                .source
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
                .or_else(|| Some(attachment.file_id.to_string()))
        });
        let data_base64 = attachment.read().ok().map(|bytes| {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(bytes)
        });
        Self {
            file_id: attachment.file_id,
            mime_type: attachment.mime_type.clone(),
            filename,
            source: attachment.source.clone(),
            data_base64,
        }
    }
}

async fn dispatch_channel_event(
    channel_runtime: &LoadedChannel,
    channel: &str,
    config_json: &str,
    event: &Event,
) {
    let event_json = match channel_outbound_event_json(event) {
        Ok(event_json) => event_json,
        Err(err) => {
            eprintln!("failed to serialize {channel} outbound event: {err}");
            return;
        }
    };

    let handled = channel_runtime
        .wasm
        .lock()
        .await
        .on_outbound_event(&event_json, config_json);
    match handled {
        Ok(true) => eprintln!("bridge: {channel} channel handled outbound event"),
        Ok(false) => eprintln!("{channel} channel did not handle outbound event"),
        Err(err) => eprintln!("{channel} outbound event failed: {err}"),
    }
}

fn channel_outbound_event_json(event: &Event) -> Result<String, String> {
    let event = ChannelOutboundEvent::try_from(event)?;
    serde_json::to_string(&event).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn outbound_message_serializes_bus_event_with_attachment_data() {
        let files_dir = unique_test_files_dir();
        fs::create_dir_all(&files_dir).unwrap();
        let source = files_dir.join("7.png");
        fs::write(&source, b"image").unwrap();
        let event = Event::MessageOut {
            channel: "telegram".to_string(),
            recipient: Sender::User,
            address: Some(ChannelAddress {
                channel: "telegram".to_string(),
                conversation_id: "chat".to_string(),
                thread_id: None,
                message_id: None,
            }),
            message: Message::Assistant {
                text: Some("hi".to_string()),
                attachments: vec![Attachment {
                    file_id: 7,
                    mime_type: "image/png".to_string(),
                    filename: None,
                    source,
                }],
                tool_calls: Vec::new(),
            },
        };

        let payload: serde_json::Value =
            serde_json::from_str(&channel_outbound_event_json(&event).unwrap()).unwrap();

        assert_eq!(payload["MessageOut"]["message"]["Assistant"]["text"], "hi");
        assert_eq!(
            payload["MessageOut"]["message"]["Assistant"]["attachments"][0]["data_base64"],
            "aW1hZ2U="
        );
        assert_eq!(
            payload["MessageOut"]["message"]["Assistant"]["attachments"][0]["filename"],
            "7.png"
        );
    }

    #[test]
    fn delete_message_serializes_as_delete_event_not_fake_message() {
        let event = Event::DeleteMessage {
            channel: "telegram".to_string(),
            address: ChannelAddress {
                channel: "telegram".to_string(),
                conversation_id: "chat".to_string(),
                thread_id: None,
                message_id: Some("42".to_string()),
            },
        };

        let payload = channel_outbound_event_json(&event).unwrap();

        assert!(payload.contains("\"DeleteMessage\""));
        assert!(payload.contains("\"message_id\":\"42\""));
        assert!(!payload.contains("delete_message_id"));
    }

    #[test]
    fn channel_action_serializes_for_outbound_channel() {
        let event = Event::ChannelAction {
            channel: "telegram".to_string(),
            recipient: Sender::User,
            address: ChannelAddress {
                channel: "telegram".to_string(),
                conversation_id: "chat".to_string(),
                thread_id: Some("7".to_string()),
                message_id: None,
            },
            action: ChannelAction::Typing,
        };

        let payload: serde_json::Value =
            serde_json::from_str(&channel_outbound_event_json(&event).unwrap()).unwrap();

        assert_eq!(
            payload["ChannelAction"]["address"]["conversation_id"],
            "chat"
        );
        assert_eq!(payload["ChannelAction"]["address"]["thread_id"], "7");
        assert_eq!(payload["ChannelAction"]["action"], "typing");
    }

    fn unique_test_files_dir() -> PathBuf {
        std::env::temp_dir().join(format!("warden-bridge-test-{}", uuid::Uuid::new_v4()))
    }
}
