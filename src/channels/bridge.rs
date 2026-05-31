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
    openai::{
        EasyInputContent, EasyInputMessage, InputContent, InputFileContent,
        InputItem, InputMessage, InputRole, InputTextContent, Role,
    },
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

pub async fn bind(addr: &str) -> io::Result<TcpListener> {
    TcpListener::bind(addr).await
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
    let event_json = wasm
        .handle_http(&path, &body, &channel_state.config_json)
        .map_err(|err| (StatusCode::BAD_REQUEST, err.to_string()))?;
    let mut emitted_messages = wasm.take_emitted_messages();
    if let Some(event_json) = event_json {
        emitted_messages.push(event_json);
    }
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
            continue;
        };
        let channel_state = channel_runtime.state.lock().await.clone();
        let Some(address) = address.or(channel_state.config.default_address) else {
            eprintln!("{channel} message out has no address and no default_address");
            continue;
        };

        let message_json = match serde_json::to_string(&message) {
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
            Ok(true) => {}
            Ok(false) => eprintln!("{channel} channel did not handle message out"),
            Err(err) => eprintln!("{channel} message out failed: {err}"),
        }
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
    content: serde_json::Value,
}

fn content_to_input_item(content: serde_json::Value, files_dir: &PathBuf) -> InputItem {
    if let Some(text) = content.as_str() {
        return EasyInputMessage {
            r#type: Default::default(),
            role: Role::User,
            content: EasyInputContent::Text(text.to_string()),
            phase: None,
        }
        .into();
    }

    let Some(parts) = content.as_array() else {
        return EasyInputMessage {
            r#type: Default::default(),
            role: Role::User,
            content: EasyInputContent::Text(content.to_string()),
            phase: None,
        }
        .into();
    };

    let input_parts = parts
        .iter()
        .filter_map(|p| content_part_to_input(p, files_dir))
        .collect::<Vec<_>>();

    InputMessage {
        role: InputRole::User,
        content: input_parts,
        status: None,
    }
    .into()
}

fn content_part_to_input(part: &serde_json::Value, files_dir: &PathBuf) -> Option<InputContent> {
    const MAX_FILE_BYTES: usize = 10 * 1024 * 1024; // 10 MB
    match part.get("type").and_then(|t| t.as_str()) {
        Some("text") => Some(InputContent::InputText(InputTextContent {
            text: part.get("text")?.as_str()?.to_string(),
        })),
        Some("image_url") => {
            let url = part
                .get("image_url")
                .and_then(|u| u.get("url"))
                .and_then(|u| u.as_str())?
                .to_string();
            // Data-URL images: store on disk and give the model a tool reference.
            if url.starts_with("data:") {
                let (ext, data) = parse_data_url(&url)?;
                if data.len() > MAX_FILE_BYTES {
                    return Some(InputContent::InputText(InputTextContent {
                        text: "[Attached image was too large to process (> 10 MB)]".to_string(),
                    }));
                }
                let file_id = format!("{}.{ext}", uuid::Uuid::new_v4());
                store_file(files_dir, &file_id, &data);
                return Some(InputContent::InputText(InputTextContent {
                    text: format!(
                        "An image is attached (file_id: '{file_id}'). \
                        Use the read_file tool to view it."
                    ),
                }));
            }
            // Regular URL: pass through as InputText so the model can see the URL.
            Some(InputContent::InputText(InputTextContent {
                text: format!("Image URL: {url}"),
            }))
        }
        Some("file") => {
            let file = part.get("file")?;
            // Inline file_data: store on disk and give the model a tool reference.
            if let Some(raw) = file.get("file_data").and_then(|d| d.as_str()) {
                let data_url = if raw.starts_with("data:") {
                    raw.to_string()
                } else {
                    format!("data:application/pdf;base64,{raw}")
                };
                let (ext, data) = parse_data_url(&data_url)?;
                if data.len() > MAX_FILE_BYTES {
                    return Some(InputContent::InputText(InputTextContent {
                        text: "[Attached file was too large to process (> 10 MB)]".to_string(),
                    }));
                }
                let base_name = file
                    .get("filename")
                    .and_then(|n| n.as_str())
                    .and_then(|n| std::path::Path::new(n).file_stem())
                    .and_then(|s| s.to_str())
                    .unwrap_or("file");
                let file_id = format!("{}-{}.{ext}", base_name, uuid::Uuid::new_v4());
                store_file(files_dir, &file_id, &data);
                return Some(InputContent::InputText(InputTextContent {
                    text: format!(
                        "A file is attached (file_id: '{file_id}'). \
                        Use the read_file tool to read it."
                    ),
                }));
            }
            // file_id / file_url reference: pass through as-is.
            Some(InputContent::InputFile(InputFileContent {
                file_data: None,
                file_id: file
                    .get("file_id")
                    .and_then(|id| id.as_str())
                    .map(str::to_string),
                file_url: file
                    .get("file_url")
                    .and_then(|u| u.as_str())
                    .map(str::to_string),
                filename: file
                    .get("filename")
                    .and_then(|n| n.as_str())
                    .map(str::to_string),
                detail: None,
            }))
        }
        _ => None,
    }
}

fn store_file(files_dir: &PathBuf, file_id: &str, data: &[u8]) {
    if let Err(e) = fs::create_dir_all(files_dir) {
        eprintln!("bridge: failed to create files_dir: {e}");
        return;
    }
    if let Err(e) = fs::write(files_dir.join(file_id), data) {
        eprintln!("bridge: failed to write file {file_id}: {e}");
    }
}

fn parse_data_url(url: &str) -> Option<(String, Vec<u8>)> {
    use base64::Engine as _;
    let rest = url.strip_prefix("data:")?;
    let (meta, b64) = rest.split_once(',')?;
    let mime = meta.split(';').next()?;
    let ext = ext_from_mime(mime);
    let data = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    Some((ext.to_string(), data))
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
