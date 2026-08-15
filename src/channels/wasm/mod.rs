use std::{path::PathBuf, sync::Arc};

use tokio::sync::{Mutex, RwLock};

use crate::{
    channels::{bridge::Bridge, manifest::ChannelManifest},
    secrets::ChannelSecrets,
};

mod inbound;
mod outbound;
mod runtime;

pub(crate) use inbound::{attachment_from_content, routes};
use runtime::WasmChannel;

#[derive(Clone)]
pub(crate) struct LoadedChannel {
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) config_path: PathBuf,
    pub(crate) state: Arc<Mutex<LoadedChannelState>>,
    pub(crate) wasm: Arc<Mutex<WasmChannel>>,
}

#[derive(Clone)]
pub(crate) struct LoadedChannelState {
    pub(crate) config: crate::channels::manifest::ChannelConfig,
    pub(crate) config_json: String,
}

pub(crate) struct WasmChannels {
    channels: RwLock<Vec<LoadedChannel>>,
    pub(super) bridge: Bridge,
}

impl WasmChannels {
    pub(crate) fn new(bridge: Bridge) -> Self {
        Self {
            channels: RwLock::new(Vec::new()),
            bridge,
        }
    }

    pub(crate) async fn add(&self, channel: LoadedChannel) {
        let path = channel.path.clone();
        let outbound = channel.clone();
        let mut channels = self.channels.write().await;
        let replaced = channels
            .iter()
            .filter(|existing| existing.name == channel.name || existing.path == path)
            .map(|existing| existing.name.clone())
            .collect::<Vec<_>>();
        channels.retain(|existing| existing.name != channel.name && existing.path != path);
        channels.push(channel);
        drop(channels);
        for name in replaced {
            self.bridge.remove_channel(&name).await;
        }
        self.bridge.add_channel(Arc::new(outbound)).await;
    }

    pub(crate) async fn remove_by_name(&self, name: &str) -> bool {
        let mut channels = self.channels.write().await;
        let before = channels.len();
        channels.retain(|channel| channel.name != name);
        let removed = channels.len() != before;
        drop(channels);
        self.bridge.remove_channel(name).await;
        removed
    }

    pub(super) async fn by_path(&self, path: &str) -> Option<LoadedChannel> {
        self.channels
            .read()
            .await
            .iter()
            .find(|channel| channel.path == path)
            .cloned()
    }
}

pub(crate) fn load_channel(
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
        .map_err(|error| error.to_string())?;
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
