use std::io;

use tokio::task::JoinHandle;

use crate::{bus::Bus, config::RuntimeLayout, secrets::ChannelSecrets};

mod bridge;
pub mod config;
pub mod ngrok;
pub mod wasm;

pub use bridge::DEFAULT_ADDR;

pub struct ChannelRuntime {
    statuses: Vec<String>,
    _bridge_task: JoinHandle<()>,
    _ngrok: Option<ngrok::ActiveNgrok>,
}

impl ChannelRuntime {
    pub fn statuses(&self) -> &[String] {
        &self.statuses
    }
}

pub async fn start(
    bus: Bus,
    layout: &RuntimeLayout,
    secrets: ChannelSecrets,
    addr: &str,
    token: String,
) -> io::Result<ChannelRuntime> {
    let listener = bridge::bind(addr).await.map_err(|err| {
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
        if let Err(err) = channel.wasm.lock().await.on_start(&channel.config_json) {
            eprintln!("{} channel start failed: {err}", channel.name);
        }
    }
    let addr = addr.to_string();
    let bridge_task = tokio::spawn(async move {
        if let Err(err) = bridge::run_server(listener, bus, &addr, token, secrets, channels).await {
            eprintln!("channel bridge failed: {err}");
        }
    });

    Ok(ChannelRuntime {
        statuses,
        _bridge_task: bridge_task,
        _ngrok: active_ngrok,
    })
}
