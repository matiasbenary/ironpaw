use std::path::PathBuf;

use serde::Serialize;

use crate::{
    bus::{ChannelAction, ConversationKey, Event, MessageRef, UserInputRequest},
    channels::bridge::{Channel, Delivery, DeliveryFuture},
    message::{Attachment, Message, ToolCall},
};

use super::LoadedChannel;

impl Channel for LoadedChannel {
    fn name(&self) -> &str {
        &self.name
    }

    fn deliver<'a>(&'a self, event: &'a Event) -> DeliveryFuture<'a> {
        Box::pin(async move {
            let state = self.state.lock().await.clone();
            let conversation = match event {
                Event::MessageOut {
                    channel,
                    chat_id,
                    thread_id,
                    ..
                } => message_out_conversation(
                    channel,
                    chat_id.clone(),
                    thread_id.clone(),
                    state.config.default_conversation,
                ),
                _ => None,
            };
            let event = match (event, &conversation) {
                (
                    Event::MessageOut {
                        message_id,
                        message,
                        ..
                    },
                    Some(conversation),
                ) => Event::MessageOut {
                    channel: self.name.clone(),
                    chat_id: Some(conversation.chat_id.clone()),
                    thread_id: conversation.thread_id.clone(),
                    message_id: message_id.clone(),
                    message: message.clone(),
                },
                (Event::MessageOut { .. }, None) => {
                    return Err("message has no conversation or default conversation".to_string());
                }
                _ => event.clone(),
            };
            let event_json = outbound_event_json(&event)?;
            let external_id = self
                .wasm
                .lock()
                .await
                .on_outbound_event(&event_json, &state.config_json)
                .map_err(|error| error.to_string())?
                .filter(|id| !id.is_empty());
            Ok(Delivery {
                conversation,
                external_id,
            })
        })
    }
}

#[derive(Serialize)]
enum ChannelOutboundEvent {
    MessageOut {
        channel: String,
        conversation: Option<ConversationKey>,
        #[serde(skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
        message: ChannelOutboundMessage,
    },
    DeleteMessage {
        channel: String,
        message: MessageRef,
    },
    ChannelAction {
        channel: String,
        conversation: ConversationKey,
        action: ChannelAction,
    },
    UserInputRequested {
        channel: String,
        conversation: ConversationKey,
        request: UserInputRequest,
    },
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

#[derive(Serialize)]
struct ChannelOutboundAttachment {
    mime_type: String,
    filename: String,
    source: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    data_base64: Option<String>,
}

impl From<&Attachment> for ChannelOutboundAttachment {
    fn from(attachment: &Attachment) -> Self {
        let data_base64 = attachment.read().ok().map(|bytes| {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(bytes)
        });
        Self {
            mime_type: attachment.mime_type.clone(),
            filename: attachment.filename.clone(),
            source: attachment.source.clone(),
            data_base64,
        }
    }
}

fn outbound_event_json(event: &Event) -> Result<String, String> {
    let event = match event {
        Event::MessageOut {
            channel,
            chat_id,
            thread_id,
            message_id,
            message:
                Message::Assistant {
                    text,
                    attachments,
                    tool_calls,
                },
        } => ChannelOutboundEvent::MessageOut {
            channel: channel.clone(),
            conversation: chat_id.as_ref().map(|chat_id| ConversationKey {
                channel: channel.clone(),
                chat_id: chat_id.clone(),
                thread_id: thread_id.clone(),
            }),
            message_id: message_id.clone(),
            message: ChannelOutboundMessage::Assistant {
                text: text.clone(),
                attachments: attachments
                    .iter()
                    .map(ChannelOutboundAttachment::from)
                    .collect(),
                tool_calls: tool_calls.clone(),
            },
        },
        Event::MessageOut { .. } => {
            return Err("message is not deliverable to a channel".to_string());
        }
        Event::DeleteMessage { message } => ChannelOutboundEvent::DeleteMessage {
            channel: message.conversation.channel.clone(),
            message: message.clone(),
        },
        Event::ChannelAction {
            conversation,
            action,
        } => ChannelOutboundEvent::ChannelAction {
            channel: conversation.channel.clone(),
            conversation: conversation.clone(),
            action: action.clone(),
        },
        Event::UserInputRequested {
            conversation,
            request,
        } => ChannelOutboundEvent::UserInputRequested {
            channel: conversation.channel.clone(),
            conversation: conversation.clone(),
            request: request.clone(),
        },
        _ => return Err("event is not deliverable to a channel".to_string()),
    };
    serde_json::to_string(&event).map_err(|error| error.to_string())
}

fn message_out_conversation(
    channel: &str,
    chat_id: Option<String>,
    thread_id: Option<String>,
    default_conversation: Option<ConversationKey>,
) -> Option<ConversationKey> {
    chat_id
        .map(|chat_id| ConversationKey {
            channel: channel.to_string(),
            chat_id,
            thread_id,
        })
        .or(default_conversation)
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
            chat_id: Some("chat".to_string()),
            thread_id: None,
            message_id: None,
            message: Message::Assistant {
                text: Some("hi".to_string()),
                attachments: vec![Attachment {
                    filename: "image.png".to_string(),
                    mime_type: "image/png".to_string(),
                    source,
                }],
                tool_calls: Vec::new(),
            },
        };
        let payload: serde_json::Value =
            serde_json::from_str(&outbound_event_json(&event).unwrap()).unwrap();
        assert_eq!(payload["MessageOut"]["message"]["Assistant"]["text"], "hi");
        assert_eq!(
            payload["MessageOut"]["message"]["Assistant"]["attachments"][0]["data_base64"],
            "aW1hZ2U="
        );
    }

    #[test]
    fn delete_message_serializes_as_delete_event_not_fake_message() {
        let event = Event::DeleteMessage {
            message: MessageRef {
                conversation: ConversationKey {
                    channel: "telegram".to_string(),
                    chat_id: "chat".to_string(),
                    thread_id: None,
                },
                message_id: "42".to_string(),
            },
        };
        let payload = outbound_event_json(&event).unwrap();
        assert!(payload.contains("\"DeleteMessage\""));
        assert!(payload.contains("\"message_id\":\"42\""));
    }

    fn unique_test_files_dir() -> PathBuf {
        std::env::temp_dir().join(format!("ironpaw-wasm-test-{}", uuid::Uuid::new_v4()))
    }
}
