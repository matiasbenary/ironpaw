wit_bindgen::generate!({
    world: "channel",
    path: "wit/channel.wit",
});

use std::{cell::Cell, net::Ipv4Addr};

use serde::Deserialize;

use crate::ironpaw::channel::channel_host;

thread_local! {
    /// Sequential id assigned to each outbound assistant message so it can
    /// later be targeted by a `DeleteMessage` event. WASM components run
    /// single-threaded, so a plain `Cell` is enough.
    static NEXT_MESSAGE_ID: Cell<u32> = const { Cell::new(0) };
}

fn next_message_id() -> u32 {
    NEXT_MESSAGE_ID.with(|next| {
        let id = next.get() + 1;
        next.set(id);
        id
    })
}

fn validate_callback_url(callback_url: &str) -> Result<(), String> {
    let Some(address) = callback_url.strip_prefix("http://") else {
        return Err("callback URL must use http".to_string());
    };
    let Some((host, port)) = address.rsplit_once(':') else {
        return Err("callback URL must include a port".to_string());
    };
    let host: Ipv4Addr = host
        .parse()
        .map_err(|_| "callback URL host must be an IPv4 address".to_string())?;
    if !host.is_loopback() {
        return Err("callback URL host must be loopback".to_string());
    }
    let port: u16 = port
        .parse()
        .map_err(|_| "callback URL port must be valid".to_string())?;
    if port == 0 {
        return Err("callback URL port must be non-zero".to_string());
    }
    Ok(())
}

struct LocalClientChannel;

impl Guest for LocalClientChannel {
    fn on_start(_config_json: String) -> Result<(), String> {
        Ok(())
    }

    fn handle_http(_path: String, body_json: String, _config_json: String) -> Result<(), String> {
        let incoming: IncomingMessage =
            serde_json::from_str(&body_json).map_err(|err| err.to_string())?;

        if incoming.content.trim().is_empty() {
            return Err("message content cannot be empty".to_string());
        }
        validate_callback_url(&incoming.callback_url)?;

        let event = serde_json::json!({
            "channel": "local-client",
            "channel_user_id": "local",
            "conversation": {
                "channel": "local-client",
                "chat_id": incoming.callback_url,
                "thread_id": null
            },
            "message_id": incoming.message_id,
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

    fn on_outbound_event(
        event_json: String,
        _config_json: String,
    ) -> Result<Option<String>, String> {
        let event: OutboundEvent =
            serde_json::from_str(&event_json).map_err(|err| err.to_string())?;
        match event {
            OutboundEvent::MessageOut {
                conversation,
                message_id,
                message,
            } => {
                let Some(conversation) = conversation else {
                    return Err("local-client message out did not include conversation".to_string());
                };
                let IronpawMessage::Assistant(message) = message else {
                    return Ok(None);
                };
                let id = message_id.unwrap_or_else(|| next_message_id().to_string());
                let mut envelope = serde_json::to_value(&message).map_err(|err| err.to_string())?;
                envelope["kind"] = serde_json::json!("message");
                envelope["id"] = serde_json::json!(id);
                channel_host::http_request("POST", &conversation.chat_id, &envelope.to_string())?;
                Ok(Some(id))
            }
            OutboundEvent::DeleteMessage { message } => {
                let envelope = serde_json::json!({
                    "kind": "delete",
                    "message_id": message.message_id,
                });
                channel_host::http_request(
                    "POST",
                    &message.conversation.chat_id,
                    &envelope.to_string(),
                )?;
                Ok(Some(String::new()))
            }
            OutboundEvent::ChannelAction { .. } => Ok(None),
            OutboundEvent::UserInputRequested {
                conversation,
                request,
            } => {
                let id = next_message_id().to_string();
                let options = request
                    .options
                    .iter()
                    .map(|option| format!("- {} (`{}`)", option.label, option.value))
                    .collect::<Vec<_>>()
                    .join("\n");
                let envelope = serde_json::json!({
                    "kind": "message",
                    "id": id,
                    "content": format!("{}\n\n{}", request.question, options),
                    "tool_calls": [],
                });
                channel_host::http_request("POST", &conversation.chat_id, &envelope.to_string())?;
                Ok(Some(id))
            }
        }
    }
}

#[derive(Deserialize)]
#[allow(dead_code)]
enum OutboundEvent {
    MessageOut {
        conversation: Option<ConversationKey>,
        #[serde(default)]
        message_id: Option<String>,
        message: IronpawMessage,
    },
    DeleteMessage {
        message: MessageRef,
    },
    ChannelAction {
        conversation: ConversationKey,
        action: serde_json::Value,
    },
    UserInputRequested {
        conversation: ConversationKey,
        request: UserInputRequest,
    },
}

#[derive(Deserialize)]
#[allow(dead_code)]
enum IronpawMessage {
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
    #[serde(default)]
    message_id: Option<String>,
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

#[derive(Deserialize)]
struct UserInputRequest {
    question: String,
    options: Vec<UserInputOption>,
}

#[derive(Deserialize)]
struct UserInputOption {
    label: String,
    value: String,
}

fn serialize_content<S>(content: &Option<String>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_str(content.as_deref().unwrap_or_default())
}

export!(LocalClientChannel);

#[cfg(test)]
mod tests {
    use super::validate_callback_url;

    #[test]
    fn accepts_loopback_http_callback_url() {
        assert!(validate_callback_url("http://127.0.0.1:7878").is_ok());
    }

    #[test]
    fn rejects_non_loopback_callback_url() {
        assert!(validate_callback_url("http://example.com:7878").is_err());
        assert!(validate_callback_url("http://192.168.1.1:7878").is_err());
        assert!(validate_callback_url("https://127.0.0.1:7878").is_err());
    }
}
