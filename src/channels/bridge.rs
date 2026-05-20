use std::{collections::HashMap, io, sync::Arc};

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
        ChatCompletionRequestMessage, ChatCompletionRequestUserMessage,
        ChatCompletionRequestUserMessageContent,
    },
    secrets::ChannelSecrets,
};

pub const DEFAULT_ADDR: &str = "127.0.0.1:7878";

#[derive(Clone)]
pub struct LoadedChannel {
    pub name: String,
    pub config: ChannelConfig,
    pub config_json: String,
    pub wasm: Arc<Mutex<WasmChannel>>,
}

#[derive(Clone)]
struct BridgeState {
    bus: Bus,
    token: Arc<String>,
    secrets: ChannelSecrets,
    channels_by_path: Arc<HashMap<String, LoadedChannel>>,
}

pub async fn run_server(
    listener: TcpListener,
    bus: Bus,
    addr: &str,
    token: String,
    secrets: ChannelSecrets,
    channels: Vec<LoadedChannel>,
) -> io::Result<()> {
    let channels_by_path = channels
        .into_iter()
        .map(|channel| (channel.config.path.clone(), channel))
        .collect::<HashMap<_, _>>();
    let channels_by_name = channels_by_path
        .values()
        .cloned()
        .map(|channel| (channel.name.clone(), channel))
        .collect::<HashMap<_, _>>();
    tokio::spawn(deliver_message_out(bus.clone(), channels_by_name));

    let state = BridgeState {
        bus,
        token: Arc::new(token),
        secrets,
        channels_by_path: Arc::new(channels_by_path),
    };
    let app = Router::new()
        .route("/{*path}", post(post_channel_message))
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
        config: manifest.config,
        config_json: manifest.config_json,
        wasm: Arc::new(Mutex::new(wasm)),
    })
}

async fn post_channel_message(
    State(state): State<BridgeState>,
    Path(path): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let path = format!("/{path}");
    let Some(channel) = state.channels_by_path.get(&path).cloned() else {
        return Err((StatusCode::NOT_FOUND, "channel route not found".to_string()));
    };

    authorize(&headers, &state.token, &state.secrets, &channel.config)?;

    let body = String::from_utf8(body.to_vec())
        .map_err(|err| (StatusCode::BAD_REQUEST, err.to_string()))?;
    let mut wasm = channel.wasm.lock().await;
    let event_json = wasm
        .handle_http(&path, &body, &channel.config_json)
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

        let sender = sender_for_inbound(&channel.config, &inbound);
        if sender != Sender::User {
            ignored += 1;
            continue;
        }

        state.bus.publish(Event::MessageIn {
            channel: inbound.channel.clone(),
            sender,
            address: inbound.address,
            message: user_message(inbound.content),
        });
        accepted += 1;
    }

    Ok(Json(serde_json::json!({
        "ok": true,
        "accepted": accepted,
        "ignored": ignored
    })))
}

async fn deliver_message_out(bus: Bus, channels_by_name: HashMap<String, LoadedChannel>) {
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
            &channel_runtime.config_json,
        );
        match handled {
            Ok(true) => {}
            Ok(false) => eprintln!("{channel} channel did not handle message out"),
            Err(err) => eprintln!("{channel} message out failed: {err}"),
        }
    }
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
    content: ChatCompletionRequestUserMessageContent,
}

fn user_message(content: ChatCompletionRequestUserMessageContent) -> ChatCompletionRequestMessage {
    ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
        content,
        name: None,
    })
}
