use std::{collections::HashMap, fs, io, path::PathBuf, sync::Arc};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    routing::post,
};
use serde::Deserialize;
use tokio::{net::TcpListener, sync::Mutex};

use crate::{
    bus::{Bus, ChannelAddress, Event, Sender},
    channels::{
        config::{ChannelAuth, ChannelConfig, ChannelManifest},
        wasm::WasmChannel,
    },
    message::{Attachment, Message},
    secrets::ChannelSecrets,
};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
struct OutboundAttachment {
    kind: String,
    mime_type: String,
    filename: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    data_base64: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize)]
struct OutboundMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    attachments: Vec<OutboundAttachment>,
}

pub const DEFAULT_ADDR: &str = "127.0.0.1:7878";

#[derive(Clone)]
pub struct LoadedChannel {
    pub name: String,
    pub config_path: std::path::PathBuf,
    pub state: Arc<Mutex<LoadedChannelState>>,
    pub wasm: Arc<Mutex<WasmChannel>>,
}

#[derive(Clone)]
pub struct LoadedChannelState {
    pub config: ChannelConfig,
    pub config_json: String,
}

#[derive(Clone)]
struct BridgeState {
    bus: Bus,
    token: Arc<String>,
    secrets: ChannelSecrets,
    channels_by_path: Arc<HashMap<String, LoadedChannel>>,
    files_dir: PathBuf,
}

pub async fn run_server(
    listener: TcpListener,
    bus: Bus,
    addr: &str,
    token: String,
    secrets: ChannelSecrets,
    channels: Vec<LoadedChannel>,
    files_dir: PathBuf,
) -> io::Result<()> {
    let mut channels_by_path = HashMap::new();
    for channel in channels {
        let path = channel.state.lock().await.config.path.clone();
        channels_by_path.insert(path, channel);
    }
    let channels_by_name = channels_by_path
        .values()
        .cloned()
        .map(|channel| (channel.name.clone(), channel))
        .collect::<HashMap<_, _>>();
    tokio::spawn(message_warden_to_channel(bus.clone(), channels_by_name));

    let state = BridgeState {
        bus,
        token: Arc::new(token),
        secrets,
        channels_by_path: Arc::new(channels_by_path),
        files_dir,
    };
    let app = Router::new()
        .route("/{*path}", post(message_channel_to_warden))
        .with_state(state);

    println!("channel bridge listening on http://{addr}");

    axum::serve(listener, app).await.map_err(io::Error::other)
}

pub fn load_channel(
    manifest: ChannelManifest,
    secrets: ChannelSecrets,
) -> Result<LoadedChannel, String> {
    let credentials = manifest
        .config
        .http_credentials
        .clone()
        .into_iter()
        .map(|credential| credential.into_injection())
        .collect();
    let wasm = WasmChannel::load(&manifest.wasm_path, secrets, credentials)
        .map_err(|err| err.to_string())?;
    Ok(LoadedChannel {
        name: manifest.name,
        config_path: manifest.config_path,
        state: Arc::new(Mutex::new(LoadedChannelState {
            config: manifest.config,
            config_json: manifest.config_json,
        })),
        wasm: Arc::new(Mutex::new(wasm)),
    })
}

async fn message_channel_to_warden(
    State(state): State<BridgeState>,
    Path(path): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let path = format!("/{path}");
    let Some(channel) = state.channels_by_path.get(&path).cloned() else {
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
    };

    let mut accepted = 0;
    let mut ignored = 0;
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
        if sender != Sender::User {
            ignored += 1;
            continue;
        }

        remember_default_address(&channel, &inbound.address)
            .await
            .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err))?;

        state.bus.publish(Event::MessageIn {
            channel: inbound.channel.clone(),
            sender,
            address: inbound.address,
            message: content_to_input_item(inbound.content, &state.files_dir),
        });
        accepted += 1;
    }

    Ok(Json(serde_json::json!({
        "ok": true,
        "accepted": accepted,
        "ignored": ignored
    })))
}

async fn message_warden_to_channel(bus: Bus, channels_by_name: HashMap<String, LoadedChannel>) {
    let mut events = bus.subscribe();
    loop {
        let event = match events.recv().await {
            Ok(Event::MessageOut {
                channel,
                recipient: Sender::User,
                address,
                message,
            }) => Event::MessageOut {
                channel,
                recipient: Sender::User,
                address,
                message,
            },
            Ok(_) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        };

        let Event::MessageOut {
            channel,
            address,
            message,
            ..
        } = event
        else {
            continue;
        };
        let Some(channel_runtime) = channels_by_name.get(&channel) else {
            eprintln!("bridge: no loaded channel named {channel} for message out");
            continue;
        };
        let channel_state = channel_runtime.state.lock().await.clone();
        let Some(address) = address.or(channel_state.config.default_address) else {
            eprintln!("{channel} message out has no address and no default_address");
            continue;
        };
        eprintln!(
            "bridge: delivering message out to channel {channel} conversation_id={}",
            address.conversation_id
        );

        let Some(outbound_message) = channel_outbound_message(message) else {
            eprintln!("{channel} message out was not deliverable to a channel");
            continue;
        };
        let message_json = match serde_json::to_string(&outbound_message) {
            Ok(message_json) => message_json,
            Err(err) => {
                eprintln!("failed to serialize {channel} message out: {err}");
                continue;
            }
        };
        let address_json = match serde_json::to_string(&address) {
            Ok(address_json) => address_json,
            Err(err) => {
                eprintln!("failed to serialize {channel} address: {err}");
                continue;
            }
        };

        let handled = channel_runtime.wasm.lock().await.on_message_out(
            &message_json,
            &address_json,
            &channel_state.config_json,
        );
        match handled {
            Ok(true) => eprintln!("bridge: {channel} channel handled message out"),
            Ok(false) => eprintln!("{channel} channel did not handle message out"),
            Err(err) => eprintln!("{channel} message out failed: {err}"),
        }
    }
}

fn channel_outbound_message(message: Message) -> Option<OutboundMessage> {
    let Message::Assistant {
        text, attachments, ..
    } = message
    else {
        return None;
    };
    Some(OutboundMessage {
        content: text,
        attachments: attachments
            .into_iter()
            .map(channel_outbound_attachment)
            .collect(),
    })
}

fn channel_outbound_attachment(attachment: Attachment) -> OutboundAttachment {
    let Attachment {
        file_id,
        mime_type,
        filename,
        source,
    } = attachment;
    let data_base64 = fs::read(&source).ok().map(|bytes| {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    });
    OutboundAttachment {
        kind: if mime_type.starts_with("image/") {
            "image".to_string()
        } else {
            "file".to_string()
        },
        mime_type,
        filename: filename.unwrap_or_else(|| {
            source
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
                .unwrap_or_else(|| file_id.to_string())
        }),
        file_id: Some(file_id),
        data_base64,
        url: None,
    }
}

async fn remember_default_address(
    channel: &LoadedChannel,
    address: &ChannelAddress,
) -> Result<(), String> {
    let mut state = channel.state.lock().await;
    if state.config.default_address.as_ref() == Some(address) {
        return Ok(());
    }

    state.config.default_address = Some(address.clone());
    let mut config_json = serde_json::from_str::<serde_json::Value>(&state.config_json)
        .map_err(|err| err.to_string())?;
    config_json["default_address"] =
        serde_json::to_value(address).map_err(|err| err.to_string())?;
    state.config_json =
        serde_json::to_string_pretty(&config_json).map_err(|err| err.to_string())?;
    fs::write(&channel.config_path, &state.config_json).map_err(|err| err.to_string())
}

fn authorize(
    headers: &HeaderMap,
    token: &str,
    secrets: &ChannelSecrets,
    config: &ChannelConfig,
) -> Result<(), (StatusCode, String)> {
    match config.auth {
        ChannelAuth::None => Ok(()),
        ChannelAuth::Bearer => authorize_bearer(headers, token),
        ChannelAuth::HeaderSecret {
            ref header,
            ref value,
            ref secret,
        } => {
            let expected_value = match (secret.as_deref(), value.as_deref()) {
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
            authorize_header_secret(headers, header, &expected_value)
        }
    }
}

fn authorize_bearer(headers: &HeaderMap, token: &str) -> Result<(), (StatusCode, String)> {
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return Err((
            StatusCode::UNAUTHORIZED,
            "missing Authorization header".to_string(),
        ));
    };
    let value = value.to_str().map_err(|_| {
        (
            StatusCode::UNAUTHORIZED,
            "invalid Authorization header".to_string(),
        )
    })?;

    if value == format!("Bearer {token}") {
        Ok(())
    } else {
        Err((StatusCode::UNAUTHORIZED, "invalid bearer token".to_string()))
    }
}

fn authorize_header_secret(
    headers: &HeaderMap,
    header_name: &str,
    expected_value: &str,
) -> Result<(), (StatusCode, String)> {
    let Some(value) = headers.get(header_name) else {
        return Err((
            StatusCode::UNAUTHORIZED,
            format!("missing {header_name} header"),
        ));
    };
    let value = value.to_str().map_err(|_| {
        (
            StatusCode::UNAUTHORIZED,
            format!("invalid {header_name} header"),
        )
    })?;

    if value == expected_value {
        Ok(())
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            format!("invalid {header_name} header"),
        ))
    }
}

fn sender_for_inbound(config: &ChannelConfig, inbound: &ChannelInbound) -> Sender {
    if matches!(config.auth, ChannelAuth::Bearer) {
        return Sender::User;
    }

    if config.owner_id.as_deref() == Some(inbound.channel_user_id.as_str()) {
        Sender::User
    } else {
        Sender::Guest
    }
}

#[derive(Deserialize)]
struct ChannelInbound {
    channel: String,
    channel_user_id: String,
    address: ChannelAddress,
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

fn content_to_input_item(content: Vec<InboundContent>, files_dir: &PathBuf) -> Message {
    let mut text_parts = Vec::new();
    let mut attachments = Vec::new();
    for part in content {
        match content_part_to_message(part, files_dir) {
            InboundMessagePart::Text(text) => text_parts.push(text),
            InboundMessagePart::Attachment(attachment) => attachments.push(attachment),
        }
    }
    Message::User {
        text: text_parts.join("\n\n"),
        attachments,
    }
}

enum InboundMessagePart {
    Text(String),
    Attachment(Attachment),
}

fn content_part_to_message(part: InboundContent, files_dir: &PathBuf) -> InboundMessagePart {
    const MAX_FILE_BYTES: usize = 10 * 1024 * 1024; // 10 MB
    match part {
        InboundContent::Text { text } => InboundMessagePart::Text(text),
        InboundContent::Attachment {
            filename,
            mime_type,
            data_base64,
        } => {
            let Some(data) = decode_base64(&data_base64) else {
                return InboundMessagePart::Text(
                    "[Attached file was not valid base64]".to_string(),
                );
            };
            if data.len() > MAX_FILE_BYTES {
                return InboundMessagePart::Text(
                    "[Attached file was too large to process (> 10 MB)]".to_string(),
                );
            }

            let ext = attachment_ext(filename.as_deref(), &mime_type);
            let file_id = received_file_id();
            let source = store_file(files_dir, file_id, &ext, &data);
            InboundMessagePart::Attachment(Attachment {
                file_id,
                mime_type,
                filename,
                source,
            })
        }
    }
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
        let part = InboundContent::Attachment {
            filename: Some("photo.png".to_string()),
            mime_type: "image/png".to_string(),
            data_base64: "aW1hZ2U=".to_string(),
        };

        let input = content_part_to_message(part, &files_dir);

        let file_id = attachment_file_id(&input);
        let stored = files_dir.join(format!("{file_id}.png"));
        assert_eq!(stored.extension().and_then(|ext| ext.to_str()), Some("png"));
        assert_eq!(fs::read(stored).unwrap(), b"image");
    }

    #[test]
    fn file_attachment_is_stored_with_numeric_id() {
        let files_dir = unique_test_files_dir();
        let part = InboundContent::Attachment {
            filename: Some("report.pdf".to_string()),
            mime_type: "application/pdf".to_string(),
            data_base64: "cGRm".to_string(),
        };

        let input = content_part_to_message(part, &files_dir);

        let file_id = attachment_file_id(&input);
        let stored = files_dir.join(format!("{file_id}.pdf"));
        assert_eq!(stored.extension().and_then(|ext| ext.to_str()), Some("pdf"));
        assert_eq!(fs::read(stored).unwrap(), b"pdf");
    }

    fn unique_test_files_dir() -> PathBuf {
        std::env::temp_dir().join(format!("warden-bridge-test-{}", uuid::Uuid::new_v4()))
    }

    fn attachment_file_id(input: &InboundMessagePart) -> u32 {
        match input {
            InboundMessagePart::Attachment(attachment) => attachment.file_id,
            InboundMessagePart::Text(_) => panic!("expected attachment"),
        }
    }
}
