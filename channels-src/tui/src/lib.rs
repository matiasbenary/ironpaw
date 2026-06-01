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

    fn on_message_out(
        message_json: String,
        address_json: String,
        _config_json: String,
    ) -> Result<bool, String> {
        let address: ChannelAddress =
            serde_json::from_str(&address_json).map_err(|err| err.to_string())?;
        channel_host::http_request("POST", &address.conversation_id, &message_json)?;
        Ok(true)
    }
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

export!(TuiChannel);
