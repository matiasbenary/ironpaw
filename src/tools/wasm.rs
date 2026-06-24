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
    http::{CredentialInjection, HttpTool},
    message::ToolDefinition,
    secrets::ToolSecrets,
    tools::{InternalTool, ToolContext, ToolOutput},
};

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
    credentials: Vec<CredentialInjectionConfig>,
}

impl WasmTool {
    pub fn from_manifest(manifest: WasmToolManifest) -> Result<Self, String> {
        let mut loaded = LoadedTool::load(
            &manifest.wasm_path,
            ToolSecrets,
            manifest.config.http_credentials.clone(),
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
            guest_config_json: serde_json::to_string(&manifest.config.config)
                .map_err(|err| err.to_string())?,
            credentials: manifest.config.http_credentials,
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
        _ctx: &ToolContext,
    ) -> Result<ToolOutput, String> {
        let args_json = serde_json::to_string(&args).map_err(|err| err.to_string())?;
        let mut loaded = LoadedTool::load(&self.wasm_path, ToolSecrets, self.credentials.clone())?;
        let output = loaded.execute(&args_json, &self.guest_config_json)?;
        serde_json::from_str(&output).map_err(|err| format!("invalid WASM tool output: {err}"))
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
    pub http_credentials: Vec<CredentialInjectionConfig>,
    #[serde(default)]
    pub config: serde_json::Value,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct CredentialInjectionConfig {
    pub host: String,
    pub secret: String,
    #[serde(default)]
    pub placeholder: Option<String>,
    #[serde(default)]
    pub header: Option<String>,
    #[serde(default)]
    pub prefix: Option<String>,
}

impl CredentialInjectionConfig {
    pub fn into_injection(self) -> CredentialInjection {
        if let Some(placeholder) = self.placeholder {
            return CredentialInjection::url_placeholder(self.host, placeholder, self.secret);
        }

        CredentialInjection {
            host: self.host,
            secret: self.secret,
            header: self.header.unwrap_or_else(|| "Authorization".to_string()),
            prefix: self.prefix.unwrap_or_else(|| "Bearer ".to_string()),
            placeholder: None,
        }
    }
}

impl From<CredentialInjectionConfig> for CredentialInjection {
    fn from(value: CredentialInjectionConfig) -> Self {
        value.into_injection()
    }
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
        credentials: Vec<CredentialInjectionConfig>,
    ) -> Result<Self, String> {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);

        let engine = wasmtime::Engine::new(&config).map_err(|err| err.to_string())?;
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
                http: HttpTool::new(
                    secrets,
                    credentials
                        .into_iter()
                        .map(CredentialInjectionConfig::into_injection)
                        .collect(),
                ),
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

    fn execute(&mut self, args_json: &str, config_json: &str) -> Result<String, String> {
        self.bindings
            .call_execute(&mut self.store, args_json, config_json)
            .map_err(|err| err.to_string())?
            .map_err(|err| format!("WASM tool error: {err}"))
    }
}

struct StoreData {
    ctx: WasiCtx,
    table: ResourceTable,
    http: HttpTool<ToolSecrets>,
}

impl warden::tool::tool_host::Host for StoreData {
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
}

impl WasiView for StoreData {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}
