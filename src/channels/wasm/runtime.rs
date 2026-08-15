use std::{fmt, path::Path};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use wasmtime::{
    Store,
    component::{Component, Linker},
};
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::{network::http::NetworkGate, secrets::ChannelSecrets};

wasmtime::component::bindgen!({
    world: "channel",
    path: "wit/channel.wit",
});

pub(crate) struct WasmChannel {
    store: Store<StoreData>,
    bindings: Channel,
}

impl WasmChannel {
    pub(crate) fn load(
        path: impl AsRef<Path>,
        secrets: ChannelSecrets,
        credentials: Vec<crate::network::http::NetworkGrant>,
    ) -> Result<Self, WasmChannelError> {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        let engine = wasmtime::Engine::new(&config)
            .map_err(|error| WasmChannelError::Load(error.to_string()))?;
        let component = Component::from_file(&engine, path)
            .map_err(|error| WasmChannelError::Load(error.to_string()))?;
        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker)
            .map_err(|error| WasmChannelError::Load(error.to_string()))?;
        Channel::add_to_linker::<StoreData, wasmtime::component::HasSelf<StoreData>>(
            &mut linker,
            |state| state,
        )
        .map_err(|error| WasmChannelError::Load(error.to_string()))?;
        let mut store = Store::new(
            &engine,
            StoreData {
                ctx: WasiCtxBuilder::new().build(),
                table: ResourceTable::new(),
                http: NetworkGate::new(secrets, credentials),
                emitted_messages: Vec::new(),
            },
        );
        let bindings = Channel::instantiate(&mut store, &component, &linker)
            .map_err(|error| WasmChannelError::Load(error.to_string()))?;
        Ok(Self { store, bindings })
    }

    pub(crate) fn on_start(&mut self, config_json: &str) -> Result<(), WasmChannelError> {
        self.bindings
            .call_on_start(&mut self.store, config_json)
            .map_err(|error| WasmChannelError::Call(error.to_string()))?
            .map_err(WasmChannelError::Guest)
    }

    pub(crate) fn handle_http(
        &mut self,
        path: &str,
        body_json: &str,
        config_json: &str,
    ) -> Result<(), WasmChannelError> {
        self.bindings
            .call_handle_http(&mut self.store, path, body_json, config_json)
            .map_err(|error| WasmChannelError::Call(error.to_string()))?
            .map_err(WasmChannelError::Guest)
    }

    pub(crate) fn on_outbound_event(
        &mut self,
        event_json: &str,
        config_json: &str,
    ) -> Result<Option<String>, WasmChannelError> {
        self.bindings
            .call_on_outbound_event(&mut self.store, event_json, config_json)
            .map_err(|error| WasmChannelError::Call(error.to_string()))?
            .map_err(WasmChannelError::Guest)
    }

    pub(crate) fn take_emitted_messages(&mut self) -> Vec<String> {
        std::mem::take(&mut self.store.data_mut().emitted_messages)
    }
}

#[derive(Debug)]
pub(crate) enum WasmChannelError {
    Load(String),
    Call(String),
    Guest(String),
}

impl fmt::Display for WasmChannelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load(message) => write!(formatter, "failed to load WASM channel: {message}"),
            Self::Call(message) => write!(formatter, "failed to call WASM channel: {message}"),
            Self::Guest(message) => write!(formatter, "WASM channel rejected message: {message}"),
        }
    }
}

impl std::error::Error for WasmChannelError {}

struct StoreData {
    ctx: WasiCtx,
    table: ResourceTable,
    http: NetworkGate<ChannelSecrets>,
    emitted_messages: Vec<String>,
}

impl ironpaw::channel::channel_host::Host for StoreData {
    fn http_request(
        &mut self,
        method: String,
        url: String,
        body_json: String,
    ) -> Result<String, String> {
        if method.to_uppercase() != "POST" {
            return Err("only POST is supported by channel http-request for now".to_string());
        }
        self.http
            .post_json_string_blocking(&url, &body_json)
            .map_err(|error| error.to_string())
    }

    fn http_request_base64(
        &mut self,
        method: String,
        url: String,
        body_json: String,
    ) -> Result<String, String> {
        self.http
            .request_bytes_string_blocking(&method, &url, &body_json)
            .map(|bytes| STANDARD.encode(bytes))
            .map_err(|error| error.to_string())
    }

    fn http_multipart_base64(
        &mut self,
        method: String,
        url: String,
        fields_json: String,
        file_field: String,
        filename: String,
        mime_type: String,
        data_base64: String,
    ) -> Result<String, String> {
        if method.to_uppercase() != "POST" {
            return Err("only POST is supported by channel http-multipart-base64".to_string());
        }
        self.http
            .post_multipart_base64_blocking(
                &url,
                &fields_json,
                &file_field,
                &filename,
                &mime_type,
                &data_base64,
            )
            .map_err(|error| error.to_string())
    }

    fn emit_message(&mut self, message_json: String) -> Result<(), String> {
        self.emitted_messages.push(message_json);
        Ok(())
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
