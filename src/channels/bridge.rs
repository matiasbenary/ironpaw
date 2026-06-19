use std::{collections::HashMap, fs, io, path::PathBuf, sync::Arc};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    routing::post,
};
use serde::Deserialize;
use tokio::{
    net::TcpListener,
    sync::{Mutex, RwLock},
};

use crate::{
    bus::{Bus, ChannelAddress, Event, Sender},
    channels::{
        config::{ChannelAuth, ChannelConfig, ChannelManifest},
        wasm::WasmChannel,
    },
    message::{Attachment, Message},
    secrets::ChannelSecrets,
};

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
    registry: Arc<ChannelRegistry>,
    files_dir: PathBuf,
}

#[derive(Default)]
pub struct ChannelRegistry {
    channels_by_path: RwLock<HashMap<String, LoadedChannel>>,
    channels_by_name: RwLock<HashMap<String, LoadedChannel>>,
}

impl ChannelRegistry {
    pub async fn new(channels: Vec<LoadedChannel>) -> Self {
        let registry = Self::default();
        for channel in channels {
            registry.add(channel).await;
        }
        registry
    }

    pub async fn add(&self, channel: LoadedChannel) {
        let path = channel.state.lock().await.config.path.clone();
        self.channels_by_path
            .write()
            .await
            .insert(path, channel.clone());
        self.channels_by_name
            .write()
            .await
            .insert(channel.name.clone(), channel);
    }

    async fn by_path(&self, path: &str) -> Option<LoadedChannel> {
        self.channels_by_path.read().await.get(path).cloned()
    }

    async fn by_name(&self, name: &str) -> Option<LoadedChannel> {
        self.channels_by_name.read().await.get(name).cloned()
    }
}

pub async fn run_server(
    listener: TcpListener,
    bus: Bus,
    addr: &str,
    token: String,
    secrets: ChannelSecrets,
    registry: Arc<ChannelRegistry>,
    files_dir: PathBuf,
) -> io::Result<()> {
    tokio::spawn(message_warden_to_channel(
        bus.clone(),
        Arc::clone(&registry),
    ));

    let state = BridgeState {
        bus,
        token: Arc::new(token),
        secrets,
        registry,
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

async fn message_warden_to_channel(bus: Bus, registry: Arc<ChannelRegistry>) {
    let mut events = bus.subscribe();
    loop {
        let (channel, event) = match events.recv().await {
            Ok(Event::MessageOut {
                channel,
                recipient: Sender::User,
                address,
                message,
            }) => {
                let Some(channel_runtime) = registry.by_name(&channel).await else {
                    eprintln!("bridge: no loaded channel named {channel} for message out");
                    continue;
                };
                let channel_state = channel_runtime.state.lock().await.clone();
                let Some(address) = address.or(channel_state.config.default_address) else {
                    eprintln!("{channel} message out has no address and no default_address");
                    continue;
                };
                if !matches!(message, Message::Assistant { .. }) {
                    eprintln!("{channel} message out was not deliverable to a channel");
                    continue;
                }
                (
                    channel.clone(),
                    Event::MessageOut {
                        channel,
                        recipient: Sender::User,
                        address: Some(address),
                        message,
                    },
                )
            }
            Ok(Event::DeleteMessage { channel, address }) => {
                if address.message_id.is_none() {
                    eprintln!("{channel} delete message has no message_id");
                    continue;
                }
                (channel.clone(), Event::DeleteMessage { channel, address })
            }
            Ok(Event::ChannelAction {
                channel,
                recipient: Sender::User,
                address,
                action,
            }) => (
                channel.clone(),
                Event::ChannelAction {
                    channel,
                    recipient: Sender::User,
                    address,
                    action,
                },
            ),
            Ok(_) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        };
        let Some(channel_runtime) = registry.by_name(&channel).await else {
            eprintln!("bridge: no loaded channel named {channel} for outbound event");
            continue;
        };
        let channel_state = channel_runtime.state.lock().await.clone();
        dispatch_channel_event(
            &channel_runtime,
            &channel,
            &channel_state.config_json,
            &event,
        )
        .await;
    }
}

async fn dispatch_channel_event(
    channel_runtime: &LoadedChannel,
    channel: &str,
    config_json: &str,
    event: &Event,
) {
    let event_json = match channel_outbound_event_json(event) {
        Ok(event_json) => event_json,
        Err(err) => {
            eprintln!("failed to serialize {channel} outbound event: {err}");
            return;
        }
    };

    let handled = channel_runtime
        .wasm
        .lock()
        .await
        .on_outbound_event(&event_json, config_json);
    match handled {
        Ok(true) => eprintln!("bridge: {channel} channel handled outbound event"),
        Ok(false) => eprintln!("{channel} channel did not handle outbound event"),
        Err(err) => eprintln!("{channel} outbound event failed: {err}"),
    }
}

fn channel_outbound_event_json(event: &Event) -> Result<String, serde_json::Error> {
    let mut value = serde_json::to_value(event)?;
    add_attachment_data(&mut value);
    serde_json::to_string(&value)
}

fn add_attachment_data(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                add_attachment_data(item);
            }
        }
        serde_json::Value::Object(object) => {
            if object.contains_key("file_id")
                && object.contains_key("mime_type")
                && object.contains_key("source")
            {
                add_attachment_payload(object);
            }
            for child in object.values_mut() {
                add_attachment_data(child);
            }
        }
        _ => {}
    }
}

fn add_attachment_payload(object: &mut serde_json::Map<String, serde_json::Value>) {
    let Some(source) = object
        .get("source")
        .and_then(|source| source.as_str())
        .map(str::to_string)
    else {
        return;
    };
    if !object.contains_key("filename") {
        let filename = std::path::Path::new(&source)
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
            .or_else(|| {
                object
                    .get("file_id")
                    .and_then(|file_id| file_id.as_u64())
                    .map(|file_id| file_id.to_string())
            });
        if let Some(filename) = filename {
            object.insert("filename".to_string(), serde_json::Value::String(filename));
        }
    }
    if let Ok(bytes) = fs::read(source) {
        use base64::Engine as _;
        object.insert(
            "data_base64".to_string(),
            serde_json::Value::String(base64::engine::general_purpose::STANDARD.encode(bytes)),
        );
    }
}

async fn remember_default_address(
    channel: &LoadedChannel,
    address: &ChannelAddress,
) -> Result<(), String> {
    let mut default_address = address.clone();
    default_address.message_id = None;
    let mut state = channel.state.lock().await;
    if state.config.default_address.as_ref() == Some(&default_address) {
        return Ok(());
    }

    state.config.default_address = Some(default_address.clone());
    let mut config_json = serde_json::from_str::<serde_json::Value>(&state.config_json)
        .map_err(|err| err.to_string())?;
    config_json["default_address"] =
        serde_json::to_value(&default_address).map_err(|err| err.to_string())?;
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

    #[test]
    fn outbound_message_serializes_bus_event_with_attachment_data() {
        let files_dir = unique_test_files_dir();
        fs::create_dir_all(&files_dir).unwrap();
        let source = files_dir.join("7.png");
        fs::write(&source, b"image").unwrap();
        let event = Event::MessageOut {
            channel: "telegram".to_string(),
            recipient: Sender::User,
            address: Some(ChannelAddress {
                channel: "telegram".to_string(),
                conversation_id: "chat".to_string(),
                thread_id: None,
                message_id: None,
            }),
            message: Message::Assistant {
                text: Some("hi".to_string()),
                attachments: vec![Attachment {
                    file_id: 7,
                    mime_type: "image/png".to_string(),
                    filename: None,
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
            "7.png"
        );
    }

    #[test]
    fn delete_message_serializes_as_delete_event_not_fake_message() {
        let event = Event::DeleteMessage {
            channel: "telegram".to_string(),
            address: ChannelAddress {
                channel: "telegram".to_string(),
                conversation_id: "chat".to_string(),
                thread_id: None,
                message_id: Some("42".to_string()),
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
            channel: "telegram".to_string(),
            recipient: Sender::User,
            address: ChannelAddress {
                channel: "telegram".to_string(),
                conversation_id: "chat".to_string(),
                thread_id: Some("7".to_string()),
                message_id: None,
            },
            action: crate::bus::ChannelAction::Typing,
        };

        let payload: serde_json::Value =
            serde_json::from_str(&channel_outbound_event_json(&event).unwrap()).unwrap();

        assert_eq!(
            payload["ChannelAction"]["address"]["conversation_id"],
            "chat"
        );
        assert_eq!(payload["ChannelAction"]["address"]["thread_id"], "7");
        assert_eq!(payload["ChannelAction"]["action"], "typing");
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
