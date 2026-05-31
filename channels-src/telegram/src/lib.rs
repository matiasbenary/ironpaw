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
        config_json: String,
    ) -> Result<Option<String>, String> {
        let config: RuntimeConfig =
            serde_json::from_str(&config_json).map_err(|err| err.to_string())?;
        let update: TelegramUpdate =
            serde_json::from_str(&body_json).map_err(|err| err.to_string())?;
        let Some(message) = update.message.or(update.edited_message) else {
            return Ok(None);
        };
        let Some(from) = &message.from else {
            return Ok(None);
        };

        if from.is_bot {
            return Ok(None);
        }

        let text = message
            .text
            .as_deref()
            .or(message.caption.as_deref())
            .unwrap_or_default()
            .trim()
            .to_string();
        let attachments = message.attachments();
        if text.is_empty() && attachments.is_empty() {
            return Ok(None);
        }
        let content = warden_user_content(text, attachments, &config)?;

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

        if let Some(image) = message.image_attachment() {
            send_image(
                &bot_token_placeholder,
                &address,
                image,
                if text == "<empty assistant message>" {
                    None
                } else {
                    Some(text.as_str())
                },
            )?;
            return Ok(true);
        }

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
    #[serde(default)]
    attachments: Vec<AssistantAttachment>,
}

impl AssistantMessage {
    fn image_attachment(&self) -> Option<&AssistantAttachment> {
        self.attachments
            .iter()
            .find(|attachment| attachment.kind == "image")
    }
}

#[derive(Deserialize)]
struct AssistantAttachment {
    kind: String,
    mime_type: String,
    filename: String,
    data_base64: Option<String>,
    url: Option<String>,
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
    photo: Vec<TelegramPhotoSize>,
    document: Option<TelegramDocument>,
    #[serde(default)]
    message_thread_id: Option<i64>,
}

impl TelegramMessage {
    fn attachments(&self) -> Vec<TelegramAttachment> {
        let mut attachments = Vec::new();
        if let Some(photo) = self
            .photo
            .iter()
            .max_by_key(|photo| photo.file_size.unwrap_or(photo.width * photo.height))
        {
            attachments.push(TelegramAttachment {
                file_id: photo.file_id.clone(),
                mime_type: "image/jpeg".to_string(),
                filename: "telegram-photo.jpg".to_string(),
            });
        }
        if let Some(document) = &self.document {
            let mime_type = document
                .mime_type
                .clone()
                .unwrap_or_else(|| "application/octet-stream".to_string());
            if mime_type.starts_with("image/") || mime_type == "application/pdf" {
                attachments.push(TelegramAttachment {
                    file_id: document.file_id.clone(),
                    filename: document
                        .file_name
                        .clone()
                        .unwrap_or_else(|| default_filename(&mime_type).to_string()),
                    mime_type,
                });
            }
        }
        attachments
    }
}

#[derive(Deserialize)]
struct TelegramPhotoSize {
    file_id: String,
    width: i64,
    height: i64,
    file_size: Option<i64>,
}

#[derive(Deserialize)]
struct TelegramDocument {
    file_id: String,
    file_name: Option<String>,
    mime_type: Option<String>,
}

struct TelegramAttachment {
    file_id: String,
    filename: String,
    mime_type: String,
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

fn send_image(
    bot_token_placeholder: &str,
    address: &ChannelAddress,
    image: &AssistantAttachment,
    caption: Option<&str>,
) -> Result<(), String> {
    if let Some(url) = &image.url {
        let mut body = serde_json::json!({
            "chat_id": address.conversation_id,
            "photo": url,
        });
        if let Some(caption) = caption {
            body["caption"] = serde_json::json!(caption);
        }
        if let Some(thread_id) = &address.thread_id {
            let message_thread_id = thread_id.parse::<i64>().map_err(|err| err.to_string())?;
            body["message_thread_id"] = serde_json::json!(message_thread_id);
        }
        channel_host::http_request(
            "POST",
            &format!("https://api.telegram.org/bot{bot_token_placeholder}/sendPhoto"),
            &body.to_string(),
        )?;
        return Ok(());
    }

    let Some(data_base64) = &image.data_base64 else {
        return Err("image attachment did not include url or data_base64".to_string());
    };
    let mut fields = serde_json::json!({
        "chat_id": address.conversation_id,
    });
    if let Some(caption) = caption {
        fields["caption"] = serde_json::json!(caption);
    }
    if let Some(thread_id) = &address.thread_id {
        let message_thread_id = thread_id.parse::<i64>().map_err(|err| err.to_string())?;
        fields["message_thread_id"] = serde_json::json!(message_thread_id);
    }

    channel_host::http_multipart_base64(
        "POST",
        &format!("https://api.telegram.org/bot{bot_token_placeholder}/sendPhoto"),
        &fields.to_string(),
        "photo",
        &image.filename,
        &image.mime_type,
        data_base64,
    )?;
    Ok(())
}

fn warden_user_content(
    text: String,
    attachments: Vec<TelegramAttachment>,
    config: &RuntimeConfig,
) -> Result<serde_json::Value, String> {
    if attachments.is_empty() {
        return Ok(serde_json::json!([
            {
                "type": "text",
                "text": text,
            }
        ]));
    }

    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(serde_json::json!({
            "type": "text",
            "text": text,
        }));
    }

    for attachment in attachments {
        let file_path = telegram_file_path(&attachment.file_id, config)?;
        parts.push(serde_json::json!({
            "type": "attachment",
            "filename": attachment.filename,
            "mime_type": attachment.mime_type,
            "data_base64": telegram_file_base64(&file_path, config)?,
        }));
    }

    Ok(serde_json::Value::Array(parts))
}

fn telegram_file_path(file_id: &str, config: &RuntimeConfig) -> Result<String, String> {
    let Some(telegram) = &config.telegram else {
        return Err("telegram config is required to download attachments".to_string());
    };
    let bot_token_placeholder = secret_placeholder(&telegram.bot_token_secret);
    let response = channel_host::http_request(
        "POST",
        &format!("https://api.telegram.org/bot{bot_token_placeholder}/getFile"),
        &serde_json::json!({ "file_id": file_id }).to_string(),
    )?;
    let response: TelegramGetFileResponse =
        serde_json::from_str(&response).map_err(|err| err.to_string())?;
    if !response.ok {
        return Err("telegram getFile failed".to_string());
    }
    response
        .result
        .and_then(|result| result.file_path)
        .ok_or_else(|| "telegram getFile response did not include file_path".to_string())
}

fn telegram_file_base64(file_path: &str, config: &RuntimeConfig) -> Result<String, String> {
    let Some(telegram) = &config.telegram else {
        return Err("telegram config is required to download attachments".to_string());
    };
    let bot_token_placeholder = secret_placeholder(&telegram.bot_token_secret);
    channel_host::http_request_base64(
        "GET",
        &format!("https://api.telegram.org/file/bot{bot_token_placeholder}/{file_path}"),
        "{}",
    )
}

#[derive(Deserialize)]
struct TelegramGetFileResponse {
    ok: bool,
    result: Option<TelegramGetFileResult>,
}

#[derive(Deserialize)]
struct TelegramGetFileResult {
    file_path: Option<String>,
}

fn default_filename(mime_type: &str) -> &str {
    match mime_type {
        "application/pdf" => "telegram-document.pdf",
        "image/png" => "telegram-image.png",
        "image/gif" => "telegram-image.gif",
        "image/webp" => "telegram-image.webp",
        _ => "telegram-file",
    }
}

export!(TelegramChannel);
