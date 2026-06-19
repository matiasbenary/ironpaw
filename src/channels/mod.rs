use std::{io, sync::Arc};

use tokio::{sync::Mutex, task::JoinHandle};

use crate::{bus::Bus, config::RuntimeLayout, secrets::ChannelSecrets};

mod bridge;
pub mod config;
pub mod ngrok;
pub mod wasm;

pub use bridge::DEFAULT_ADDR;

pub struct ChannelRuntime {
    statuses: Vec<String>,
    layout: RuntimeLayout,
    addr: String,
    secrets: ChannelSecrets,
    registry: Arc<bridge::ChannelRegistry>,
    ngrok: Arc<Mutex<Option<ngrok::ActiveNgrok>>>,
    _bridge_task: JoinHandle<()>,
}

impl ChannelRuntime {
    pub fn statuses(&self) -> &[String] {
        &self.statuses
    }

    pub async fn activate_installed_channel(
        &self,
        name: &str,
        external: bool,
    ) -> Result<(), String> {
        let public_url = self.public_url_for_activation(external).await?;
        let manifest = config::discover(&self.layout)
            .map_err(|err| err.to_string())?
            .into_iter()
            .find(|manifest| manifest.name == name)
            .ok_or_else(|| format!("installed channel not found: {name}"))?;
        let manifest = config::inject_public_url(manifest, public_url.as_deref())
            .map_err(|err| err.to_string())?;
        let channel =
            bridge::load_channel(manifest, self.secrets.clone()).map_err(|err| err.to_string())?;
        let config_json = channel.state.lock().await.config_json.clone();
        channel
            .wasm
            .lock()
            .await
            .on_start(&config_json)
            .map_err(|err| err.to_string())?;
        self.registry.add(channel).await;
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
                .ok_or_else(|| "external channel requires WARDEN_NGROK_TOKEN".to_string())?;
        let public_url = active_ngrok.public_url.clone();
        *self.ngrok.lock().await = Some(active_ngrok);
        Ok(Some(public_url))
    }
}

pub async fn start(
    bus: Bus,
    layout: &RuntimeLayout,
    secrets: ChannelSecrets,
    addr: &str,
    token: String,
) -> io::Result<ChannelRuntime> {
    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("failed to bind channel bridge at {addr}: {err}"),
        )
    })?;
    let active_ngrok = ngrok::start_if_configured(layout.root(), &secrets, addr)
        .await
        .map_err(io::Error::other)?;
    let public_url = active_ngrok
        .as_ref()
        .map(|active_ngrok| active_ngrok.public_url.as_str());
    let manifests = config::discover(layout)?
        .into_iter()
        .map(|manifest| config::inject_public_url(manifest, public_url))
        .collect::<io::Result<Vec<_>>>()?;
    let mut statuses = manifests
        .iter()
        .map(|manifest| {
            format!(
                "{} -> {} ({})",
                manifest.config.path,
                manifest.wasm_path.display(),
                manifest.config_path.display()
            )
        })
        .collect::<Vec<_>>();
    if let Some(public_url) = public_url {
        statuses.push(format!("ngrok -> {public_url}"));
    }
    let channels = manifests
        .into_iter()
        .map(|manifest| bridge::load_channel(manifest, secrets.clone()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(io::Error::other)?;
    for channel in &channels {
        let config_json = channel.state.lock().await.config_json.clone();
        if let Err(err) = channel.wasm.lock().await.on_start(&config_json) {
            eprintln!("{} channel start failed: {err}", channel.name);
        }
    }
    let registry = Arc::new(bridge::ChannelRegistry::new(channels).await);
    let bridge_addr = addr.to_string();
    let runtime_addr = bridge_addr.clone();
    let bridge_secrets = secrets.clone();
    let files_dir = layout.files_dir();
    let bridge_registry = Arc::clone(&registry);
    let bridge_task = tokio::spawn(async move {
        if let Err(err) = bridge::run_server(
            listener,
            bus,
            &bridge_addr,
            token,
            bridge_secrets,
            bridge_registry,
            files_dir,
        )
        .await
        {
            eprintln!("channel bridge failed: {err}");
        }
    });

    Ok(ChannelRuntime {
        statuses,
        layout: layout.clone(),
        addr: runtime_addr,
        secrets: secrets.clone(),
        registry,
        ngrok: Arc::new(Mutex::new(active_ngrok)),
        _bridge_task: bridge_task,
    })
}
