use std::{
    fs, io,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use wasmtime::{
    Store,
    component::{Component, Linker},
};
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::{
    config::RuntimeLayout,
    http::NetworkGate,
    message::ToolDefinition,
    network::email::protocol,
    secrets::ToolSecrets,
    tools::{ExecutionContext, InternalTool, ToolError, ToolResponder},
};

pub use crate::http::NetworkGrantConfig;

wasmtime::component::bindgen!({
    world: "tool",
    path: "wit/tool.wit",
});

pub struct WasmTool {
    name: String,
    description: String,
    parameters: serde_json::Value,
    wasm_path: PathBuf,
    guest_config_json: String,
    config: WasmToolConfig,
    secrets: ToolSecrets,
}

impl WasmTool {
    pub fn from_manifest(manifest: WasmToolManifest, secrets: ToolSecrets) -> Result<Self, String> {
        let mut loaded = LoadedTool::load(
            &manifest.wasm_path,
            secrets.clone(),
            manifest.config.clone(),
        )?;
        let name = loaded.name()?;
        if name != manifest.name {
            return Err(format!(
                "WASM tool '{}' exported name '{name}'",
                manifest.name
            ));
        }
        let description = loaded.description()?;
        let parameters_json = loaded.parameters()?;
        let parameters = serde_json::from_str(&parameters_json)
            .map_err(|err| format!("invalid parameters schema for {name}: {err}"))?;
        Ok(Self {
            name,
            description,
            parameters,
            wasm_path: manifest.wasm_path,
            guest_config_json: manifest.config_json,
            config: manifest.config,
            secrets,
        })
    }
}

#[async_trait]
impl InternalTool for WasmTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn tool_definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ExecutionContext,
        responder: ToolResponder,
    ) {
        let result: Result<serde_json::Value, String> = async {
            let args_json = serde_json::to_string(&args).map_err(|err| err.to_string())?;
            let wasm_path = self.wasm_path.clone();
            let secrets = self.secrets.clone();
            let config = self.config.clone();
            let guest_config_json = self.guest_config_json.clone();
            let output = tokio::task::spawn_blocking(move || {
                let mut loaded = LoadedTool::load(&wasm_path, secrets, config)?;
                loaded.execute(&args_json, &guest_config_json)
            })
            .await
            .map_err(|err| err.to_string())??;
            Ok(output)
        }
        .await;
        responder.respond(result.map_err(ToolError::new));
    }
}

pub struct WasmToolManifest {
    pub name: String,
    pub wasm_path: PathBuf,
    pub config_path: PathBuf,
    pub config_json: String,
    pub config: WasmToolConfig,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct WasmToolConfig {
    #[serde(default)]
    pub http_credentials: Vec<NetworkGrantConfig>,
    #[serde(default)]
    pub email: Option<EmailCapabilityConfig>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct EmailCapabilityConfig {
    #[serde(default)]
    pub imap: Option<EmailServerConfig>,
    #[serde(default)]
    pub smtp: Option<EmailServerConfig>,
    pub username: String,
    pub password_secret: String,
    #[serde(default)]
    pub from: Option<String>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct EmailServerConfig {
    pub host: String,
    pub port: u16,
    #[serde(default = "default_email_tls")]
    pub tls: EmailTlsMode,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EmailTlsMode {
    Tls,
    StartTls,
}

fn default_email_tls() -> EmailTlsMode {
    EmailTlsMode::Tls
}

pub fn discover(layout: &RuntimeLayout) -> io::Result<Vec<WasmToolManifest>> {
    let mut manifests = Vec::new();
    let tools_dir = layout.tools_dir();

    if !tools_dir.exists() {
        return Ok(manifests);
    }

    for entry in fs::read_dir(&tools_dir)? {
        let entry = entry?;
        let wasm_path = entry.path();
        if wasm_path
            .extension()
            .and_then(|extension| extension.to_str())
            != Some("wasm")
        {
            continue;
        }

        let Some(name) = wasm_path
            .file_stem()
            .and_then(|file_stem| file_stem.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        let config_path = tools_dir.join(format!("{name}.config.json"));
        if !config_path.exists() {
            continue;
        }

        let config_json = fs::read_to_string(&config_path)?;
        let config: WasmToolConfig =
            serde_json::from_str(&config_json).map_err(io::Error::other)?;
        manifests.push(WasmToolManifest {
            name,
            wasm_path,
            config_path,
            config_json,
            config,
        });
    }

    manifests.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(manifests)
}

struct LoadedTool {
    store: Store<StoreData>,
    bindings: Tool,
}

impl LoadedTool {
    fn load(
        path: impl AsRef<Path>,
        secrets: ToolSecrets,
        tool_config: WasmToolConfig,
    ) -> Result<Self, String> {
        let mut wasmtime_config = wasmtime::Config::new();
        wasmtime_config.wasm_component_model(true);

        let engine = wasmtime::Engine::new(&wasmtime_config).map_err(|err| err.to_string())?;
        let component = Component::from_file(&engine, path).map_err(|err| err.to_string())?;
        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker).map_err(|err| err.to_string())?;
        Tool::add_to_linker::<StoreData, wasmtime::component::HasSelf<StoreData>>(
            &mut linker,
            |state| state,
        )
        .map_err(|err| err.to_string())?;
        let mut store = Store::new(
            &engine,
            StoreData {
                ctx: WasiCtxBuilder::new().build(),
                table: ResourceTable::new(),
                http: NetworkGate::new(
                    secrets.clone(),
                    tool_config
                        .http_credentials
                        .clone()
                        .into_iter()
                        .map(Into::into)
                        .collect(),
                ),
                email: EmailHost::new(secrets, tool_config.email),
            },
        );
        let bindings =
            Tool::instantiate(&mut store, &component, &linker).map_err(|err| err.to_string())?;

        Ok(Self { store, bindings })
    }

    fn name(&mut self) -> Result<String, String> {
        self.bindings
            .call_name(&mut self.store)
            .map_err(|err| err.to_string())
    }

    fn description(&mut self) -> Result<String, String> {
        self.bindings
            .call_description(&mut self.store)
            .map_err(|err| err.to_string())
    }

    fn parameters(&mut self) -> Result<String, String> {
        self.bindings
            .call_parameters(&mut self.store)
            .map_err(|err| err.to_string())
    }

    fn execute(&mut self, args_json: &str, config_json: &str) -> Result<serde_json::Value, String> {
        let output = self
            .bindings
            .call_execute(&mut self.store, args_json, config_json)
            .map_err(|err| err.to_string())?
            .map_err(|err| format!("WASM tool error: {err}"))?;
        serde_json::from_str(&output).map_err(|err| format!("invalid WASM tool output: {err}"))
    }
}

struct StoreData {
    ctx: WasiCtx,
    table: ResourceTable,
    http: NetworkGate<ToolSecrets>,
    email: EmailHost,
}

impl ironpaw::tool::tool_host::Host for StoreData {
    fn http_request(
        &mut self,
        method: String,
        url: String,
        body_json: String,
    ) -> Result<String, String> {
        self.http
            .request_bytes_string_blocking(&method, &url, &body_json)
            .and_then(|bytes| {
                String::from_utf8(bytes).map_err(|err| {
                    crate::http::HttpError::Http(format!("response was not UTF-8: {err}"))
                })
            })
            .map_err(|err| err.to_string())
    }

    fn http_request_headers(
        &mut self,
        method: String,
        url: String,
        headers_json: String,
        body_json: String,
    ) -> Result<String, String> {
        self.http
            .request_bytes_headers_string_blocking(&method, &url, &headers_json, &body_json)
            .and_then(|bytes| {
                String::from_utf8(bytes).map_err(|err| {
                    crate::http::HttpError::Http(format!("response was not UTF-8: {err}"))
                })
            })
            .map_err(|err| err.to_string())
    }

    fn imap_list(&mut self, folder: String, limit: u32) -> Result<String, String> {
        self.email.list(&folder, limit)
    }

    fn imap_read(&mut self, folder: String, uid: String) -> Result<String, String> {
        self.email.read(&folder, &uid)
    }

    fn smtp_send(&mut self, message_json: String) -> Result<String, String> {
        self.email.send(&message_json)
    }
}

#[derive(Clone)]
struct EmailHost {
    secrets: ToolSecrets,
    config: Option<EmailCapabilityConfig>,
}

impl EmailHost {
    fn new(secrets: ToolSecrets, config: Option<EmailCapabilityConfig>) -> Self {
        Self { secrets, config }
    }

    fn list(&self, folder: &str, limit: u32) -> Result<String, String> {
        let config = self.config()?;
        let imap = config
            .imap
            .as_ref()
            .ok_or_else(|| "email IMAP capability is not configured".to_string())?;
        protocol::list(
            &self.secrets,
            &config.password_secret,
            &config.username,
            &imap.host,
            imap.port,
            matches!(imap.tls, EmailTlsMode::StartTls),
            folder,
            limit,
        )
    }

    fn read(&self, folder: &str, uid: &str) -> Result<String, String> {
        let config = self.config()?;
        let imap = config
            .imap
            .as_ref()
            .ok_or_else(|| "email IMAP capability is not configured".to_string())?;
        let uid = parse_uid(uid)?;
        protocol::read(
            &self.secrets,
            &config.password_secret,
            &config.username,
            &imap.host,
            imap.port,
            matches!(imap.tls, EmailTlsMode::StartTls),
            folder,
            uid,
        )
    }

    fn send(&self, message_json: &str) -> Result<String, String> {
        let config = self.config()?;
        let smtp = config
            .smtp
            .as_ref()
            .ok_or_else(|| "email SMTP capability is not configured".to_string())?;
        protocol::send(
            &self.secrets,
            &config.password_secret,
            &config.username,
            config.from.as_deref(),
            &smtp.host,
            smtp.port,
            matches!(smtp.tls, EmailTlsMode::StartTls),
            message_json,
        )
    }

    fn config(&self) -> Result<&EmailCapabilityConfig, String> {
        self.config
            .as_ref()
            .ok_or_else(|| "email capability is not configured for this tool".to_string())
    }
}

fn parse_uid(uid: &str) -> Result<u32, String> {
    uid.trim()
        .parse::<u32>()
        .map_err(|_| format!("invalid email UID: {uid}"))
}

impl WasiView for StoreData {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}
