use std::{path::PathBuf, sync::Arc};

use serde::Serialize;

use crate::{
    bus::{Bus, ChannelAction, ConversationKey, Event, MessageRef, UserInputRequest},
    channels::bridge::{ChannelRegistry, LoadedChannel, history::append_channel_history},
    history::History,
    message::{Attachment, Message, ToolCall},
};

pub(super) async fn message_ironpaw_to_channel(
    bus: Bus,
    history: Arc<dyn History>,
    registry: Arc<ChannelRegistry>,
) -> Result<(), String> {
    let mut events = bus.subscribe();
    loop {
        match events.recv().await {
            Ok(Event::MessageOut {
                channel,
                chat_id,
                thread_id,
                message_id,
                message,
            }) => {
                let Some(channel_runtime) = registry.by_name(&channel).await else {
                    eprintln!("bridge: no loaded channel named {channel} for message out");
                    continue;
                };
                let channel_state = channel_runtime.state.lock().await.clone();
                let Some(conversation) = message_out_conversation(
                    &channel,
                    chat_id,
                    thread_id,
                    channel_state.config.default_conversation,
                ) else {
                    eprintln!(
                        "{channel} message out has no conversation and no default_conversation"
                    );
                    continue;
                };
                if !matches!(message, Message::Assistant { .. }) {
                    eprintln!("{channel} message out was not deliverable to a channel");
                    continue;
                }
                let event = Event::MessageOut {
                    channel: channel.clone(),
                    chat_id: Some(conversation.chat_id.clone()),
                    thread_id: conversation.thread_id.clone(),
                    message_id,
                    message: message.clone(),
                };
                let result = dispatch_channel_event(
                    &channel_runtime,
                    &channel,
                    &channel_state.config_json,
                    &event,
                )
                .await;
                let external_id = result.ok().flatten().filter(|id| !id.is_empty());
                if let Err(err) =
                    append_channel_history(history.as_ref(), &conversation, external_id, &message)
                {
                    eprintln!("failed to persist {channel} outbound history: {err}");
                }
            }
            Ok(Event::DeleteMessage { message }) => {
                let channel = message.conversation.channel.clone();
                let Some(channel_runtime) = registry.by_name(&channel).await else {
                    eprintln!("bridge: no loaded channel named {channel} for delete message");
                    continue;
                };
                if let Err(err) = history.delete_external_message(&message) {
                    eprintln!("failed to delete {channel} history entry: {err}");
                }
                let channel_state = channel_runtime.state.lock().await.clone();
                let event = Event::DeleteMessage { message };
                let _ = dispatch_channel_event(
                    &channel_runtime,
                    &channel,
                    &channel_state.config_json,
                    &event,
                )
                .await;
            }
            Ok(Event::ChannelAction {
                conversation,
                action,
            }) => {
                let channel = conversation.channel.clone();
                let Some(channel_runtime) = registry.by_name(&channel).await else {
                    eprintln!("bridge: no loaded channel named {channel} for channel action");
                    continue;
                };
                let channel_state = channel_runtime.state.lock().await.clone();
                let event = Event::ChannelAction {
                    conversation,
                    action,
                };
                let _ = dispatch_channel_event(
                    &channel_runtime,
                    &channel,
                    &channel_state.config_json,
                    &event,
                )
                .await;
            }
            Ok(Event::UserInputRequested {
                conversation,
                request,
            }) => {
                let channel = conversation.channel.clone();
                let Some(channel_runtime) = registry.by_name(&channel).await else {
                    eprintln!("bridge: no loaded channel named {channel} for user input request");
                    continue;
                };
                let channel_state = channel_runtime.state.lock().await.clone();
                let event = Event::UserInputRequested {
                    conversation,
                    request,
                };
                let _ = dispatch_channel_event(
                    &channel_runtime,
                    &channel,
                    &channel_state.config_json,
                    &event,
                )
                .await;
            }
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
    Ok(())
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

async fn dispatch_channel_event(
    channel_runtime: &LoadedChannel,
    channel: &str,
    config_json: &str,
    event: &Event,
) -> Result<Option<String>, String> {
    let event_json = match channel_outbound_event_json(event) {
        Ok(event_json) => event_json,
        Err(err) => {
            eprintln!("failed to serialize {channel} outbound event: {err}");
            return Err(err);
        }
    };

    let handled = channel_runtime
        .wasm
        .lock()
        .await
        .on_outbound_event(&event_json, config_json);
    match handled {
        Ok(Some(id)) => {
            eprintln!("bridge: {channel} channel handled outbound event");
            Ok(Some(id))
        }
        Ok(None) => {
            eprintln!("{channel} channel did not handle outbound event");
            Ok(None)
        }
        Err(err) => {
            eprintln!("{channel} outbound event failed: {err}");
            Err(err.to_string())
        }
    }
}

fn channel_outbound_event_json(event: &Event) -> Result<String, String> {
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
    serde_json::to_string(&event).map_err(|err| err.to_string())
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
            serde_json::from_str(&channel_outbound_event_json(&event).unwrap()).unwrap();

        assert_eq!(payload["MessageOut"]["message"]["Assistant"]["text"], "hi");
        assert_eq!(
            payload["MessageOut"]["message"]["Assistant"]["attachments"][0]["data_base64"],
            "aW1hZ2U="
        );
        assert_eq!(
            payload["MessageOut"]["message"]["Assistant"]["attachments"][0]["filename"],
            "image.png"
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

        let payload = channel_outbound_event_json(&event).unwrap();

        assert!(payload.contains("\"DeleteMessage\""));
        assert!(payload.contains("\"message_id\":\"42\""));
        assert!(!payload.contains("delete_message_id"));
    }

    #[test]
    fn channel_action_serializes_for_outbound_channel() {
        let event = Event::ChannelAction {
            conversation: ConversationKey {
                channel: "telegram".to_string(),
                chat_id: "chat".to_string(),
                thread_id: Some("7".to_string()),
            },
            action: ChannelAction::Typing,
        };

        let payload: serde_json::Value =
            serde_json::from_str(&channel_outbound_event_json(&event).unwrap()).unwrap();

        assert_eq!(payload["ChannelAction"]["conversation"]["chat_id"], "chat");
        assert_eq!(payload["ChannelAction"]["conversation"]["thread_id"], "7");
        assert_eq!(payload["ChannelAction"]["action"], "typing");

        let progress = Event::ChannelAction {
            conversation: ConversationKey {
                channel: "telegram".to_string(),
                chat_id: "chat".to_string(),
                thread_id: None,
            },
            action: ChannelAction::Progress {
                text: "calling web_search".to_string(),
            },
        };
        let payload: serde_json::Value =
            serde_json::from_str(&channel_outbound_event_json(&progress).unwrap()).unwrap();
        assert_eq!(
            payload["ChannelAction"]["action"]["progress"]["text"],
            "calling web_search"
        );
    }

    #[test]
    fn user_input_request_serializes_for_outbound_channel() {
        let event = Event::UserInputRequested {
            conversation: ConversationKey {
                channel: "telegram".to_string(),
                chat_id: "chat".to_string(),
                thread_id: None,
            },
            request: UserInputRequest {
                question: "Choose".to_string(),
                options: vec![crate::bus::UserInputOption {
                    label: "First".to_string(),
                    value: "first".to_string(),
                }],
            },
        };

        let payload: serde_json::Value =
            serde_json::from_str(&channel_outbound_event_json(&event).unwrap()).unwrap();

        assert_eq!(
            payload["UserInputRequested"]["conversation"]["chat_id"],
            "chat"
        );
        assert_eq!(
            payload["UserInputRequested"]["request"]["question"],
            "Choose"
        );
        assert_eq!(
            payload["UserInputRequested"]["request"]["options"][0]["value"],
            "first"
        );
    }

    fn unique_test_files_dir() -> PathBuf {
        std::env::temp_dir().join(format!("ironpaw-bridge-test-{}", uuid::Uuid::new_v4()))
    }
}
