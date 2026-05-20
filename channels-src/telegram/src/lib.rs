wit_bindgen::generate!({
    world: "channel",
    path: "../../wit/channel.wit",
});

use serde::Deserialize;

use crate::warden::channel::channel_host;

struct TelegramChannel;

impl Guest for TelegramChannel {
    fn on_start(config_json: String) -> Result<(), String> {
        let config: RuntimeConfig =
            serde_json::from_str(&config_json).map_err(|err| err.to_string())?;
        let Some(telegram) = config.telegram else {
            return Ok(());
        };
        if !telegram.webhook_enabled {
            return Ok(());
        }
        let bot_token_placeholder = secret_placeholder(&telegram.bot_token_secret);
        let public_url = config.public_url.ok_or_else(|| {
            "telegram webhook is enabled but no public URL is configured".to_string()
        })?;
        let webhook_url = format!(
            "{}/{}",
            public_url.trim_end_matches('/'),
            config.path.trim_start_matches('/')
        );
        let mut body = serde_json::json!({
            "url": webhook_url,
            "allowed_updates": ["message", "edited_message"],
        });
        if config.auth.r#type == "header_secret" {
            if let Some(value) = config.auth.value {
                body["secret_token"] = serde_json::Value::String(value);
            } else if let Some(secret) = config.auth.secret {
                body["secret_token"] = serde_json::Value::String(format!("{{{secret}}}"));
            }
        }

        channel_host::http_request(
            "POST",
            &format!("https://api.telegram.org/bot{bot_token_placeholder}/setWebhook"),
            &body.to_string(),
        )
        .map(|_| ())
    }

    fn handle_http(
        _path: String,
        body_json: String,
        _config_json: String,
    ) -> Result<Option<String>, String> {
        let update: TelegramUpdate =
            serde_json::from_str(&body_json).map_err(|err| err.to_string())?;
        let Some(message) = update.message.or(update.edited_message) else {
            return Ok(None);
        };
        let Some(from) = message.from else {
            return Ok(None);
        };

        if from.is_bot {
            return Ok(None);
        }

        let content = message
            .text
            .or(message.caption)
            .unwrap_or_default()
            .trim()
            .to_string();
        if content.is_empty() {
            return Ok(None);
        }

        let event = serde_json::json!({
            "channel": "telegram",
            "channel_user_id": from.id.to_string(),
            "address": {
                "channel": "telegram",
                "conversation_id": message.chat.id.to_string(),
                "thread_id": message.message_thread_id.map(|id| id.to_string())
            },
            "content": content,
        })
        .to_string();
        channel_host::emit_message(&event)?;
        Ok(None)
    }

    fn on_message_out(
        message_json: String,
        address_json: String,
        config_json: String,
    ) -> Result<bool, String> {
        let config: RuntimeConfig =
            serde_json::from_str(&config_json).map_err(|err| err.to_string())?;
        let Some(telegram) = config.telegram else {
            return Ok(false);
        };
        let bot_token_placeholder = secret_placeholder(&telegram.bot_token_secret);
        let address: ChannelAddress =
            serde_json::from_str(&address_json).map_err(|err| err.to_string())?;
        let message: AssistantMessage =
            serde_json::from_str(&message_json).map_err(|err| err.to_string())?;
        let text = display_message(&message);

        let mut body = serde_json::json!({
            "chat_id": address.conversation_id,
            "text": text,
        });
        if let Some(thread_id) = address.thread_id {
            let message_thread_id = thread_id.parse::<i64>().map_err(|err| err.to_string())?;
            body["message_thread_id"] = serde_json::json!(message_thread_id);
        }

        channel_host::http_request(
            "POST",
            &format!("https://api.telegram.org/bot{bot_token_placeholder}/sendMessage"),
            &body.to_string(),
        )?;
        Ok(true)
    }
}

#[derive(Deserialize)]
struct RuntimeConfig {
    path: String,
    #[serde(default)]
    auth: RuntimeAuth,
    #[serde(default)]
    public_url: Option<String>,
    #[serde(default)]
    telegram: Option<TelegramConfig>,
}

#[derive(Default, Deserialize)]
struct RuntimeAuth {
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    secret: Option<String>,
}

#[derive(Deserialize)]
struct TelegramConfig {
    bot_token_secret: String,
    #[serde(default)]
    webhook_enabled: bool,
}

#[derive(Deserialize)]
struct AssistantMessage {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<serde_json::Value>>,
}

#[derive(Deserialize)]
struct ChannelAddress {
    conversation_id: String,
    #[serde(default)]
    thread_id: Option<String>,
}

#[derive(Deserialize)]
struct TelegramUpdate {
    message: Option<TelegramMessage>,
    edited_message: Option<TelegramMessage>,
}

#[derive(Deserialize)]
struct TelegramMessage {
    from: Option<TelegramUser>,
    chat: TelegramChat,
    text: Option<String>,
    caption: Option<String>,
    #[serde(default)]
    message_thread_id: Option<i64>,
}

#[derive(Deserialize)]
struct TelegramChat {
    id: i64,
}

#[derive(Deserialize)]
struct TelegramUser {
    id: i64,
    is_bot: bool,
}

fn display_message(message: &AssistantMessage) -> String {
    if let Some(content) = &message.content {
        return content.clone();
    }

    if let Some(tool_calls) = &message.tool_calls {
        return format!("assistant requested {} tool call(s)", tool_calls.len());
    }

    "<empty assistant message>".to_string()
}

fn secret_placeholder(secret: &str) -> String {
    format!("{{{secret}}}")
}

export!(TelegramChannel);
