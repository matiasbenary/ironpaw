wit_bindgen::generate!({
    world: "channel",
    path: "../../wit/channel.wit",
});

use serde::Deserialize;

use crate::warden::channel::channel_host;

struct TuiChannel;

impl Guest for TuiChannel {
    fn on_start(_config_json: String) -> Result<(), String> {
        Ok(())
    }

    fn handle_http(_path: String, body_json: String, _config_json: String) -> Result<(), String> {
        let incoming: IncomingMessage =
            serde_json::from_str(&body_json).map_err(|err| err.to_string())?;

        if incoming.content.trim().is_empty() {
            return Err("message content cannot be empty".to_string());
        }

        let event = serde_json::json!({
            "channel": "tui",
            "channel_user_id": "local",
            "conversation": {
                "channel": "tui",
                "chat_id": incoming.callback_url,
                "thread_id": null
            },
            "content": [
                {
                    "type": "text",
                    "text": incoming.content
                }
            ]
        })
        .to_string();
        channel_host::emit_message(&event)
    }

    fn on_outbound_event(event_json: String, _config_json: String) -> Result<Option<String>, String> {
        let event: OutboundEvent =
            serde_json::from_str(&event_json).map_err(|err| err.to_string())?;
        match event {
            OutboundEvent::MessageOut {
                conversation,
                message,
            } => {
                let Some(conversation) = conversation else {
                    return Err("tui message out did not include conversation".to_string());
                };
                let WardenMessage::Assistant(message) = message else {
                    return Ok(None);
                };
                channel_host::http_request(
                    "POST",
                    &conversation.chat_id,
                    &serde_json::to_string(&message).map_err(|err| err.to_string())?,
                )?;
                Ok(Some(String::new()))
            }
            OutboundEvent::DeleteMessage { .. } | OutboundEvent::ChannelAction { .. } => Ok(None),
        }
    }
}

#[derive(Deserialize)]
#[allow(dead_code)]
enum OutboundEvent {
    MessageOut {
        conversation: Option<ConversationKey>,
        message: WardenMessage,
    },
    DeleteMessage {
        message: MessageRef,
    },
    ChannelAction {
        conversation: ConversationKey,
        action: String,
    },
}

#[derive(Deserialize)]
#[allow(dead_code)]
enum WardenMessage {
    Assistant(AssistantMessage),
    System(serde_json::Value),
    User(serde_json::Value),
    ToolResult(serde_json::Value),
}

#[derive(serde::Serialize, Deserialize)]
struct AssistantMessage {
    #[serde(
        rename(deserialize = "text", serialize = "content"),
        serialize_with = "serialize_content"
    )]
    content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct IncomingMessage {
    content: String,
    callback_url: String,
}

#[derive(Deserialize)]
struct ConversationKey {
    chat_id: String,
}

#[derive(Deserialize)]
struct MessageRef {
    conversation: ConversationKey,
    message_id: String,
}

fn serialize_content<S>(content: &Option<String>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(content.as_deref().unwrap_or_default())
}

export!(TuiChannel);
