use std::{io, path::PathBuf, sync::Arc};

use axum::{Router, extract::DefaultBodyLimit, routing::post};
use tokio::{
    net::TcpListener,
    sync::{Mutex, RwLock, broadcast},
};

use crate::{
    bus::Bus,
    channels::{config::ChannelManifest, wasm::WasmChannel},
    history::History,
    secrets::ChannelSecrets,
};

mod auth;
mod client;
mod history;
mod inbound;
mod outbound;

pub const DEFAULT_ADDR: &str = "127.0.0.1:7878";

#[derive(Clone)]
pub struct LoadedChannel {
    pub name: String,
    pub path: String,
    pub config_path: std::path::PathBuf,
    pub state: Arc<Mutex<LoadedChannelState>>,
    pub wasm: Arc<Mutex<WasmChannel>>,
}

#[derive(Clone)]
pub struct LoadedChannelState {
    pub config: crate::channels::config::ChannelConfig,
    pub config_json: String,
}

#[derive(Clone)]
pub(super) struct BridgeState {
    pub(super) bus: Bus,
    pub(super) history: Arc<dyn History>,
    pub(super) token: Arc<String>,
    pub(super) secrets: ChannelSecrets,
    pub(super) registry: Arc<ChannelRegistry>,
    pub(super) files_dir: PathBuf,
    pub(super) client_events: broadcast::Sender<client::ClientEvent>,
}

#[derive(Default)]
pub struct ChannelRegistry {
    channels: RwLock<Vec<LoadedChannel>>,
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
        let path = channel.path.clone();
        let mut channels = self.channels.write().await;
        channels.retain(|existing| existing.name != channel.name && existing.path != path);
        channels.push(channel);
    }

    pub async fn remove_by_name(&self, name: &str) -> bool {
        let mut channels = self.channels.write().await;
        let before = channels.len();
        channels.retain(|channel| channel.name != name);
        channels.len() != before
    }

    pub(super) async fn by_path(&self, path: &str) -> Option<LoadedChannel> {
        self.channels
            .read()
            .await
            .iter()
            .find(|channel| channel.path == path)
            .cloned()
    }

    pub(super) async fn by_name(&self, name: &str) -> Option<LoadedChannel> {
        self.channels
            .read()
            .await
            .iter()
            .find(|channel| channel.name == name)
            .cloned()
    }
}

pub async fn run_server(listener: TcpListener, addr: &str, state: BridgeState) -> io::Result<()> {
    let outbound_bus = state.bus.clone();
    let outbound_history = Arc::clone(&state.history);
    let outbound_registry = Arc::clone(&state.registry);
    tokio::spawn(async move {
        let _ =
            outbound::message_ironpaw_to_channel(outbound_bus, outbound_history, outbound_registry)
                .await;
    });

    let client_bus = state.bus.clone();
    let client_history = Arc::clone(&state.history);
    let client_events = state.client_events.clone();
    tokio::spawn(async move {
        client::forward_events(client_bus, client_history, client_events).await;
    });

    let app = Router::new()
        .merge(client::routes())
        .route(
            "/{*path}",
            post(inbound::message_channel_to_ironpaw)
                .layer(DefaultBodyLimit::max(56 * 1024 * 1024)),
        )
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
        .map(Into::into)
        .collect();
    let wasm = WasmChannel::load(&manifest.wasm_path, secrets, credentials)
        .map_err(|err| err.to_string())?;
    Ok(LoadedChannel {
        name: manifest.name,
        path: manifest.config.path.clone(),
        config_path: manifest.config_path,
        state: Arc::new(Mutex::new(LoadedChannelState {
            config: manifest.config,
            config_json: manifest.config_json,
        })),
        wasm: Arc::new(Mutex::new(wasm)),
    })
}
