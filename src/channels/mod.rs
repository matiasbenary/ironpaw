use std::{io, sync::Arc};

use tokio::{sync::Mutex, task::JoinHandle};

use crate::{
    bus::Bus, config::RuntimeLayout, history::History, inference::Inference,
    secrets::ChannelSecrets,
};

pub(crate) mod bridge;
pub(crate) mod local_client;
pub mod manifest;
pub mod ngrok;
pub mod wasm;

pub use crate::network::server::DEFAULT_ADDR;

pub struct ChannelRuntime {
    pub statuses: Mutex<Vec<String>>,
    layout: RuntimeLayout,
    addr: String,
    secrets: ChannelSecrets,
    wasm_channels: Arc<wasm::WasmChannels>,
    local_client: Arc<local_client::LocalClient>,
    ngrok: Arc<Mutex<Option<ngrok::ActiveNgrok>>>,
    server_task: JoinHandle<()>,
}

impl ChannelRuntime {
    pub fn connection_token(&self) -> &str {
        self.local_client.connection_token()
    }

    pub async fn shutdown(self) {
        self.server_task.abort();
        let _ = self.server_task.await;
    }

    pub async fn statuses(&self) -> Vec<String> {
        self.statuses.lock().await.clone()
    }

    pub async fn activate_installed_channel(
        &self,
        name: &str,
        external: bool,
    ) -> Result<(), String> {
        let public_url = self.public_url_for_activation(external).await?;
        let manifest = manifest::discover(&self.layout)
            .map_err(|err| err.to_string())?
            .into_iter()
            .find(|manifest| manifest.name == name)
            .ok_or_else(|| format!("installed channel not found: {name}"))?;
        let manifest = manifest::inject_public_url(manifest, public_url.as_deref())
            .map_err(|err| err.to_string())?;
        let channel = wasm::load_channel(manifest, self.secrets.clone())?;
        let config_json = channel.state.lock().await.config_json.clone();
        channel
            .wasm
            .lock()
            .await
            .on_start(&config_json)
            .map_err(|err| err.to_string())?;
        self.wasm_channels.add(channel).await;
        Ok(())
    }

    pub async fn remove_channel(&self, name: &str) -> bool {
        self.wasm_channels.remove_by_name(name).await
    }

    pub async fn load_installed_channels(&self) -> Result<(), String> {
        self.load_installed_channels_matching(None).await
    }

    pub async fn load_only_channels(&self, names: &[String]) -> Result<(), String> {
        self.load_installed_channels_matching(Some(names)).await
    }

    async fn load_installed_channels_matching(
        &self,
        only_names: Option<&[String]>,
    ) -> Result<(), String> {
        let manifests = manifest::discover(&self.layout)
            .map_err(|err| err.to_string())?
            .into_iter()
            .filter(|manifest| {
                only_names.is_none_or(|names| names.iter().any(|name| name == &manifest.name))
            })
            .collect::<Vec<_>>();
        let public_url = self
            .public_url_for_manifests(&manifests)
            .await
            .map_err(|err| err.to_string())?;
        let manifests = manifests
            .into_iter()
            .map(|manifest| manifest::inject_public_url(manifest, public_url.as_deref()))
            .collect::<io::Result<Vec<_>>>()
            .map_err(|err| err.to_string())?;
        let mut statuses = Vec::new();
        for manifest in manifests {
            statuses.push(format!(
                "{} -> {} ({})",
                manifest.config.path,
                manifest.wasm_path.display(),
                manifest.config_path.display()
            ));
            let channel = wasm::load_channel(manifest, self.secrets.clone())?;
            let config_json = channel.state.lock().await.config_json.clone();
            let start_result = channel.wasm.lock().await.on_start(&config_json);
            if let Err(err) = start_result {
                eprintln!("{} channel start failed: {err}", channel.name);
            }
            self.wasm_channels.add(channel).await;
        }
        if let Some(public_url) = public_url {
            statuses.push(format!("ngrok -> {public_url}"));
        }
        *self.statuses.lock().await = statuses;
        Ok(())
    }

    async fn public_url_for_activation(&self, external: bool) -> Result<Option<String>, String> {
        if !external {
            return Ok(None);
        }
        {
            let ngrok = self.ngrok.lock().await;
            if let Some(ngrok) = ngrok.as_ref() {
                return Ok(Some(ngrok.public_url.clone()));
            }
        }
        let active_ngrok =
            ngrok::start_if_configured(self.layout.root(), &self.secrets, &self.addr)
                .await
                .map_err(|err| err.to_string())?
                .ok_or_else(|| "external channel requires IRONPAW_NGROK_TOKEN".to_string())?;
        let public_url = active_ngrok.public_url.clone();
        *self.ngrok.lock().await = Some(active_ngrok);
        Ok(Some(public_url))
    }

    async fn public_url_for_manifests(
        &self,
        manifests: &[manifest::ChannelManifest],
    ) -> io::Result<Option<String>> {
        if !manifests.iter().any(|manifest| {
            manifest
                .config
                .http_credentials
                .iter()
                .any(|credential| credential.host == "api.telegram.org")
        }) {
            return Ok(None);
        }
        ngrok::start_if_configured(self.layout.root(), &self.secrets, &self.addr)
            .await
            .map_err(io::Error::other)
            .map(|active| active.map(|active_ngrok| active_ngrok.public_url))
    }
}

pub async fn start(
    bus: Bus,
    history: Arc<dyn History>,
    layout: &RuntimeLayout,
    secrets: ChannelSecrets,
    inference: Inference,
    addr: &str,
) -> io::Result<ChannelRuntime> {
    let runtime = start_bridge(bus, history, layout, secrets, inference, addr).await?;
    runtime
        .load_installed_channels()
        .await
        .map_err(io::Error::other)?;
    Ok(runtime)
}

pub async fn start_bridge(
    bus: Bus,
    history: Arc<dyn History>,
    layout: &RuntimeLayout,
    secrets: ChannelSecrets,
    inference: Inference,
    addr: &str,
) -> io::Result<ChannelRuntime> {
    let listener = crate::network::server::bind(addr).await?;
    let server_addr = addr.to_string();
    let runtime_addr = server_addr.clone();
    let files_dir = layout.files_dir();
    let auth_log_path = layout.root().join("authentication.log");
    let bridge = bridge::Bridge::new(bus, history);
    let wasm_channels = Arc::new(wasm::WasmChannels::new(bridge.clone()));
    let outbound_bridge = bridge.clone();
    tokio::spawn(async move {
        outbound_bridge.run().await;
    });

    let token = local_client::connection_token(std::env::var("IRONPAW_CLIENT_TOKEN").ok())
        .map_err(io::Error::other)?;
    let device_token = local_client::device_token(std::env::var("IRONPAW_DEVICE_TOKEN").ok())
        .map_err(io::Error::other)?;
    let local_client = local_client::LocalClient::new(
        bridge.clone(),
        files_dir.clone(),
        inference,
        token,
        device_token,
    );
    bridge
        .add_channel(Arc::clone(&local_client) as Arc<dyn bridge::Channel>)
        .await;
    let connection_token = local_client.connection_token().to_string();
    let app = axum::Router::new()
        .merge(local_client.routes())
        .merge(wasm::routes(
            secrets.clone(),
            Arc::clone(&wasm_channels),
            files_dir,
        ));
    let server_task = tokio::spawn(async move {
        if let Err(err) = crate::network::server::run(
            listener,
            &server_addr,
            app,
            auth_log_path,
            connection_token,
        )
        .await
        {
            eprintln!("network server failed: {err}");
        }
    });

    Ok(ChannelRuntime {
        statuses: Mutex::new(Vec::new()),
        layout: layout.clone(),
        addr: runtime_addr,
        secrets: secrets.clone(),
        wasm_channels,
        local_client,
        ngrok: Arc::new(Mutex::new(None)),
        server_task,
    })
}
