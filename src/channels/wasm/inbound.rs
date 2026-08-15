use std::{fs, path::PathBuf, sync::Arc};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use serde::Deserialize;

use crate::{
    bus::{ConversationKey, Sender, SenderKind},
    channels::{
        bridge::{Bridge, InboundMessage},
        manifest::{ChannelAuth, ChannelConfig},
    },
    message::{Attachment, Message, available_filename},
    secrets::ChannelSecrets,
};

use super::{LoadedChannel, WasmChannels};

#[derive(Clone)]
struct WasmState {
    secrets: ChannelSecrets,
    channels: Arc<WasmChannels>,
    files_dir: PathBuf,
}

pub(crate) fn routes(
    secrets: ChannelSecrets,
    channels: Arc<WasmChannels>,
    files_dir: PathBuf,
) -> Router {
    Router::new()
        .route("/{*path}", post(channel_request))
        .with_state(WasmState {
            secrets,
            channels,
            files_dir,
        })
}

async fn channel_request(
    State(state): State<WasmState>,
    Path(path): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let path = format!("/{path}");
    let Some(channel) = state.channels.by_path(&path).await else {
        return Err((StatusCode::NOT_FOUND, "channel route not found".to_string()));
    };
    let config = channel.state.lock().await.config.clone();
    authorize(&headers, &state.secrets, &config)?;
    let body = String::from_utf8(body.to_vec())
        .map_err(|error| (StatusCode::BAD_REQUEST, error.to_string()))?;
    receive(
        &channel,
        &path,
        &body,
        &state.channels.bridge,
        &state.files_dir,
    )
    .await
    .map(Json)
    .map_err(|error| match error {
        InboundError::BadRequest(error) => (StatusCode::BAD_REQUEST, error),
        InboundError::Internal(error) => (StatusCode::INTERNAL_SERVER_ERROR, error),
    })
}

fn authorize(
    headers: &HeaderMap,
    secrets: &ChannelSecrets,
    config: &ChannelConfig,
) -> Result<(), (StatusCode, String)> {
    match &config.auth {
        ChannelAuth::None => Ok(()),
        ChannelAuth::HeaderSecret {
            header,
            value,
            secret,
        } => {
            let expected = match (secret.as_deref(), value.as_deref()) {
                (Some(secret), _) => secrets.get(secret).ok_or_else(|| {
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        format!("missing secret: {secret}"),
                    )
                })?,
                (None, Some(value)) => value.to_string(),
                (None, None) => {
                    return Err((
                        StatusCode::SERVICE_UNAVAILABLE,
                        "header secret auth is not configured".to_string(),
                    ));
                }
            };
            crate::network::server::auth::authorize_header(headers, header, &expected)
        }
    }
}

enum InboundError {
    BadRequest(String),
    Internal(String),
}

async fn receive(
    channel: &LoadedChannel,
    path: &str,
    body: &str,
    bridge: &Bridge,
    files_dir: &PathBuf,
) -> Result<serde_json::Value, InboundError> {
    let channel_state = channel.state.lock().await.clone();
    let mut wasm = channel.wasm.lock().await;
    wasm.handle_http(path, body, &channel_state.config_json)
        .map_err(|error| InboundError::BadRequest(error.to_string()))?;
    let emitted_messages = wasm.take_emitted_messages();
    drop(wasm);

    if emitted_messages.is_empty() {
        return Ok(serde_json::json!({ "ok": true, "ignored": true }));
    }

    let mut accepted = 0;
    for event_json in emitted_messages {
        let inbound: ChannelInbound = serde_json::from_str(&event_json)
            .map_err(|error| InboundError::BadRequest(error.to_string()))?;
        if inbound.channel != channel.name {
            return Err(InboundError::BadRequest(format!(
                "{} channel emitted wrong channel",
                channel.name
            )));
        }
        let sender = sender_for_inbound(&channel_state.config, &inbound);
        if sender.kind() == SenderKind::User {
            remember_default_conversation(channel, &inbound.conversation)
                .await
                .map_err(InboundError::Internal)?;
        }
        let message = content_to_message(inbound.content, files_dir, &sender);
        bridge
            .receive(InboundMessage {
                sender,
                conversation: inbound.conversation,
                message_id: inbound.message_id,
                message,
            })
            .map_err(InboundError::Internal)?;
        accepted += 1;
    }
    Ok(serde_json::json!({ "ok": true, "accepted": accepted }))
}

async fn remember_default_conversation(
    channel: &LoadedChannel,
    conversation: &ConversationKey,
) -> Result<(), String> {
    let mut state = channel.state.lock().await;
    if state.config.default_conversation.is_some() {
        return Ok(());
    }
    state.config.default_conversation = Some(conversation.clone());
    let mut config_json = serde_json::from_str::<serde_json::Value>(&state.config_json)
        .map_err(|error| error.to_string())?;
    config_json["default_conversation"] =
        serde_json::to_value(conversation).map_err(|error| error.to_string())?;
    state.config_json =
        serde_json::to_string_pretty(&config_json).map_err(|error| error.to_string())?;
    fs::write(&channel.config_path, &state.config_json).map_err(|error| error.to_string())
}

fn sender_for_inbound(config: &ChannelConfig, inbound: &ChannelInbound) -> Sender {
    if config.owner_id.as_deref() == Some(inbound.channel_user_id.as_str()) {
        Sender::User(inbound.channel_user_id.clone())
    } else {
        Sender::Guest(inbound.channel_user_id.clone())
    }
}

#[derive(Deserialize)]
struct ChannelInbound {
    channel: String,
    channel_user_id: String,
    conversation: ConversationKey,
    #[serde(default)]
    message_id: Option<String>,
    content: Vec<InboundContent>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum InboundContent {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "attachment")]
    Attachment {
        filename: Option<String>,
        mime_type: String,
        data_base64: String,
    },
}

fn content_to_message(
    content: Vec<InboundContent>,
    files_dir: &PathBuf,
    sender: &Sender,
) -> Message {
    let mut text_parts = Vec::new();
    let mut attachments = Vec::new();
    for part in content {
        match part {
            InboundContent::Text { text } => text_parts.push(text),
            InboundContent::Attachment {
                filename,
                mime_type,
                data_base64,
            } if sender.kind() == SenderKind::User => {
                match attachment_from_content(filename, mime_type, data_base64, files_dir) {
                    Ok(attachment) => attachments.push(attachment),
                    Err(text) => text_parts.push(text),
                }
            }
            InboundContent::Attachment { .. } => {}
        }
    }
    Message::User {
        text: text_parts.join("\n\n"),
        attachments,
    }
}

pub(crate) fn attachment_from_content(
    filename: Option<String>,
    mime_type: String,
    data_base64: String,
    files_dir: &std::path::Path,
) -> Result<Attachment, String> {
    const MAX_FILE_BYTES: usize = 10 * 1024 * 1024;
    use base64::Engine as _;
    let data = base64::engine::general_purpose::STANDARD
        .decode(data_base64)
        .map_err(|_| "[Attached file was not valid base64]".to_string())?;
    if data.len() > MAX_FILE_BYTES {
        return Err("[Attached file was too large to process (> 10 MB)]".to_string());
    }
    let extension = filename
        .as_deref()
        .and_then(|name| std::path::Path::new(name).extension())
        .and_then(|extension| extension.to_str())
        .filter(|extension| {
            !extension.is_empty()
                && extension.len() <= 10
                && extension
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric())
        })
        .unwrap_or_else(|| extension_from_mime(&mime_type))
        .to_ascii_lowercase();
    let filename = available_filename(files_dir, filename.as_deref(), "attachment", &extension);
    fs::create_dir_all(files_dir)
        .map_err(|error| format!("failed to create files directory: {error}"))?;
    let source = files_dir.join(&filename);
    fs::write(&source, data).map_err(|error| format!("failed to store {filename}: {error}"))?;
    Ok(Attachment {
        filename,
        mime_type,
        source,
    })
}

fn extension_from_mime(mime: &str) -> &str {
    match mime {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        _ => "bin",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_attachment_keeps_its_filename() {
        let files_dir = unique_test_files_dir();
        let attachment = attachment_from_content(
            Some("photo.png".to_string()),
            "image/png".to_string(),
            "aW1hZ2U=".to_string(),
            &files_dir,
        )
        .unwrap();
        assert_eq!(attachment.filename, "photo.png");
        assert_eq!(fs::read(files_dir.join("photo.png")).unwrap(), b"image");
    }

    #[test]
    fn guest_attachment_is_discarded_without_writing_a_file() {
        let files_dir = unique_test_files_dir();
        let message = content_to_message(
            vec![InboundContent::Attachment {
                filename: Some("photo.png".to_string()),
                mime_type: "image/png".to_string(),
                data_base64: "aW1hZ2U=".to_string(),
            }],
            &files_dir,
            &Sender::Guest("guest".to_string()),
        );
        assert_eq!(
            message,
            Message::User {
                text: String::new(),
                attachments: Vec::new(),
            }
        );
        assert!(!files_dir.exists());
    }

    fn unique_test_files_dir() -> PathBuf {
        std::env::temp_dir().join(format!("ironpaw-wasm-test-{}", uuid::Uuid::new_v4()))
    }
}
