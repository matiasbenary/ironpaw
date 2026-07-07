use std::{fs, path::PathBuf};

use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use serde::Deserialize;

use crate::{
    bus::{ConversationKey, Event, Sender},
    channels::{
        bridge::{BridgeState, LoadedChannel, auth::authorize, history::append_channel_history},
        config::{ChannelAuth, ChannelConfig},
    },
    message::{Attachment, Message},
};

pub(super) async fn message_channel_to_warden(
    State(state): State<BridgeState>,
    Path(path): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let path = format!("/{path}");
    let Some(channel) = state.registry.by_path(&path).await else {
        return Err((StatusCode::NOT_FOUND, "channel route not found".to_string()));
    };
    let channel_state = channel.state.lock().await.clone();

    authorize(
        &headers,
        &state.token,
        &state.secrets,
        &channel_state.config,
    )?;

    let body = String::from_utf8(body.to_vec())
        .map_err(|err| (StatusCode::BAD_REQUEST, err.to_string()))?;
    let mut wasm = channel.wasm.lock().await;
    wasm.handle_http(&path, &body, &channel_state.config_json)
        .map_err(|err| (StatusCode::BAD_REQUEST, err.to_string()))?;
    let emitted_messages = wasm.take_emitted_messages();
    drop(wasm);

    if emitted_messages.is_empty() {
        return Ok(Json(serde_json::json!({ "ok": true, "ignored": true })));
    }

    let mut accepted = 0;
    for event_json in emitted_messages {
        let inbound: ChannelInbound = serde_json::from_str(&event_json)
            .map_err(|err| (StatusCode::BAD_REQUEST, err.to_string()))?;

        if inbound.channel != channel.name {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("{} channel emitted wrong channel", channel.name),
            ));
        }

        let sender = sender_for_inbound(&channel_state.config, &inbound);
        if sender.kind() == crate::bus::SenderKind::User {
            remember_default_conversation(&channel, &inbound.conversation)
                .await
                .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err))?;
        }

        let message = content_to_message(inbound.content, &state.files_dir);
        append_channel_history(
            state.history.as_ref(),
            &inbound.conversation,
            inbound.message_id.clone(),
            &message,
        )
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err))?;

        state.bus.publish(Event::MessageIn {
            sender,
            conversation: inbound.conversation,
            message_id: inbound.message_id,
            message,
        });
        accepted += 1;
    }

    Ok(Json(serde_json::json!({
        "ok": true,
        "accepted": accepted
    })))
}

async fn remember_default_conversation(
    channel: &LoadedChannel,
    conversation: &ConversationKey,
) -> Result<(), String> {
    let mut state = channel.state.lock().await;
    if state.config.default_conversation.as_ref() == Some(conversation) {
        return Ok(());
    }

    state.config.default_conversation = Some(conversation.clone());
    let mut config_json = serde_json::from_str::<serde_json::Value>(&state.config_json)
        .map_err(|err| err.to_string())?;
    config_json["default_conversation"] =
        serde_json::to_value(conversation).map_err(|err| err.to_string())?;
    state.config_json =
        serde_json::to_string_pretty(&config_json).map_err(|err| err.to_string())?;
    fs::write(&channel.config_path, &state.config_json).map_err(|err| err.to_string())
}

fn sender_for_inbound(config: &ChannelConfig, inbound: &ChannelInbound) -> Sender {
    if matches!(config.auth, ChannelAuth::Bearer) {
        return Sender::User(inbound.channel_user_id.clone());
    }

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

fn content_to_message(content: Vec<InboundContent>, files_dir: &PathBuf) -> Message {
    let mut text_parts = Vec::new();
    let mut attachments = Vec::new();
    for part in content {
        match part {
            InboundContent::Text { text } => text_parts.push(text),
            InboundContent::Attachment {
                filename,
                mime_type,
                data_base64,
            } => match attachment_from_content(filename, mime_type, data_base64, files_dir) {
                Ok(attachment) => attachments.push(attachment),
                Err(text) => text_parts.push(text),
            },
        }
    }
    Message::User {
        text: text_parts.join("\n\n"),
        attachments,
    }
}

fn attachment_from_content(
    filename: Option<String>,
    mime_type: String,
    data_base64: String,
    files_dir: &PathBuf,
) -> Result<Attachment, String> {
    const MAX_FILE_BYTES: usize = 10 * 1024 * 1024; // 10 MB
    let data = decode_base64(&data_base64)
        .ok_or_else(|| "[Attached file was not valid base64]".to_string())?;
    if data.len() > MAX_FILE_BYTES {
        return Err("[Attached file was too large to process (> 10 MB)]".to_string());
    }

    let ext = attachment_ext(filename.as_deref(), &mime_type);
    let file_id = received_file_id();
    let source = store_file(files_dir, file_id, &ext, &data);
    Ok(Attachment {
        file_id,
        mime_type,
        filename,
        source,
    })
}

fn store_file(files_dir: &PathBuf, file_id: u32, ext: &str, data: &[u8]) -> PathBuf {
    let path = files_dir.join(format!("{file_id}.{ext}"));
    if let Err(e) = fs::create_dir_all(files_dir) {
        eprintln!("bridge: failed to create files_dir: {e}");
        return path;
    }
    if let Err(e) = fs::write(&path, data) {
        eprintln!("bridge: failed to write file {file_id}: {e}");
    }
    path
}

fn received_file_id() -> u32 {
    let bytes = *uuid::Uuid::new_v4().as_bytes();
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn attachment_ext(filename: Option<&str>, mime_type: &str) -> String {
    filename
        .and_then(|name| std::path::Path::new(name).extension())
        .and_then(|ext| ext.to_str())
        .unwrap_or_else(|| ext_from_mime(mime_type))
        .to_string()
}

fn decode_base64(data_base64: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(data_base64)
        .ok()
}

fn ext_from_mime(mime: &str) -> &str {
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
    fn image_attachment_is_stored_with_numeric_id() {
        let files_dir = unique_test_files_dir();
        let attachment = attachment_from_content(
            Some("photo.png".to_string()),
            "image/png".to_string(),
            "aW1hZ2U=".to_string(),
            &files_dir,
        )
        .unwrap();

        let file_id = attachment.file_id;
        let stored = files_dir.join(format!("{file_id}.png"));
        assert_eq!(stored.extension().and_then(|ext| ext.to_str()), Some("png"));
        assert_eq!(fs::read(stored).unwrap(), b"image");
    }

    #[test]
    fn file_attachment_is_stored_with_numeric_id() {
        let files_dir = unique_test_files_dir();
        let attachment = attachment_from_content(
            Some("report.pdf".to_string()),
            "application/pdf".to_string(),
            "cGRm".to_string(),
            &files_dir,
        )
        .unwrap();

        let file_id = attachment.file_id;
        let stored = files_dir.join(format!("{file_id}.pdf"));
        assert_eq!(stored.extension().and_then(|ext| ext.to_str()), Some("pdf"));
        assert_eq!(fs::read(stored).unwrap(), b"pdf");
    }

    fn unique_test_files_dir() -> PathBuf {
        std::env::temp_dir().join(format!("warden-bridge-test-{}", uuid::Uuid::new_v4()))
    }
}
