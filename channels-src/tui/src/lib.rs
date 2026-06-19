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
            "address": {
                "channel": "tui",
                "conversation_id": incoming.callback_url,
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

    fn on_outbound_event(event_json: String, _config_json: String) -> Result<bool, String> {
        let event: OutboundEvent =
            serde_json::from_str(&event_json).map_err(|err| err.to_string())?;
        match event {
            OutboundEvent::MessageOut { address, message } => {
                let Some(address) = address else {
                    return Err("tui message out did not include address".to_string());
                };
                let WardenMessage::Assistant(message) = message else {
                    return Ok(false);
                };
                channel_host::http_request(
                    "POST",
                    &address.conversation_id,
                    &serde_json::to_string(&message).map_err(|err| err.to_string())?,
                )?;
                Ok(true)
            }
            OutboundEvent::DeleteMessage { .. } | OutboundEvent::ChannelAction { .. } => Ok(false),
        }
    }
}

#[derive(Deserialize)]
#[allow(dead_code)]
enum OutboundEvent {
    MessageOut {
        address: Option<ChannelAddress>,
        message: WardenMessage,
    },
    DeleteMessage {
        address: ChannelAddress,
    },
    ChannelAction {
        address: ChannelAddress,
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
struct ChannelAddress {
    conversation_id: String,
}

fn serialize_content<S>(content: &Option<String>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(content.as_deref().unwrap_or_default())
}

export!(TuiChannel);
