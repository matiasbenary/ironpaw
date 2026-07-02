use std::{
    fs, io,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use lettre::{
    Message, SmtpTransport, Transport, message::header::ContentType,
    transport::smtp::authentication::Credentials,
};
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
    tools::{InternalTool, ToolContext, ToolOutcome},
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
            guest_config_json: serde_json::to_string(&manifest.config.config)
                .map_err(|err| err.to_string())?,
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
        _ctx: &ToolContext,
    ) -> Result<ToolOutcome, String> {
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
        let output = serde_json::from_str(&output)
            .map_err(|err| format!("invalid WASM tool output: {err}"))?;
        Ok(ToolOutcome::Finished(output))
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
    pub email: Option<EmailCapabilityConfig>,
    #[serde(default)]
    pub config: serde_json::Value,
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
                http: HttpTool::new(
                    secrets.clone(),
                    tool_config
                        .http_credentials
                        .clone()
                        .into_iter()
                        .map(CredentialInjectionConfig::into_injection)
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
    email: EmailHost,
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

    fn email_list(&mut self, folder: String, limit: u32) -> Result<String, String> {
        self.email.list(&folder, limit)
    }

    fn email_read(&mut self, folder: String, uid: String) -> Result<String, String> {
        self.email.read(&folder, &uid)
    }

    fn email_send(&mut self, message_json: String) -> Result<String, String> {
        self.email.send(&message_json)
    }
}

#[derive(Clone)]
struct EmailHost {
    secrets: ToolSecrets,
    config: Option<EmailCapabilityConfig>,
}

#[derive(serde::Deserialize)]
struct OutboundEmail {
    to: Vec<String>,
    subject: String,
    body: String,
    #[serde(default)]
    cc: Vec<String>,
    #[serde(default)]
    bcc: Vec<String>,
    #[serde(default)]
    from: Option<String>,
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
        let mut session = self.imap_session(config, imap)?;
        session.select(folder).map_err(|err| err.to_string())?;
        let mut uids = session
            .uid_search("ALL")
            .map_err(|err| err.to_string())?
            .into_iter()
            .collect::<Vec<_>>();
        uids.sort_unstable_by(|left, right| right.cmp(left));
        uids.truncate(limit.clamp(1, 50) as usize);
        let uid_set = uids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let messages = if uid_set.is_empty() {
            Vec::new()
        } else {
            session
                .uid_fetch(uid_set, "(UID BODY.PEEK[HEADER])")
                .map_err(|err| err.to_string())?
                .iter()
                .map(|fetch| {
                    let headers = parse_headers(fetch.header().unwrap_or_default());
                    serde_json::json!({
                        "uid": fetch.uid.map(|uid| uid.to_string()).unwrap_or_default(),
                        "from": headers.get("from").cloned().unwrap_or_default(),
                        "to": headers.get("to").cloned().unwrap_or_default(),
                        "subject": headers.get("subject").cloned().unwrap_or_default(),
                        "date": headers.get("date").cloned().unwrap_or_default(),
                    })
                })
                .collect::<Vec<_>>()
        };
        let _ = session.logout();
        serde_json::to_string(&serde_json::json!({
            "folder": folder,
            "messages": messages,
        }))
        .map_err(|err| err.to_string())
    }

    fn read(&self, folder: &str, uid: &str) -> Result<String, String> {
        let config = self.config()?;
        let imap = config
            .imap
            .as_ref()
            .ok_or_else(|| "email IMAP capability is not configured".to_string())?;
        let uid = parse_uid(uid)?;
        let mut session = self.imap_session(config, imap)?;
        session.select(folder).map_err(|err| err.to_string())?;
        let fetches = session
            .uid_fetch(uid.to_string(), "(UID BODY.PEEK[])")
            .map_err(|err| err.to_string())?;
        let Some(fetch) = fetches.iter().next() else {
            return Err(format!("email UID {uid} not found in {folder}"));
        };
        let body = fetch.body().unwrap_or_default();
        let (headers, text) = split_message(body);
        let headers = parse_headers(headers);
        let _ = session.logout();
        serde_json::to_string(&serde_json::json!({
            "folder": folder,
            "uid": uid.to_string(),
            "from": headers.get("from").cloned().unwrap_or_default(),
            "to": headers.get("to").cloned().unwrap_or_default(),
            "subject": headers.get("subject").cloned().unwrap_or_default(),
            "date": headers.get("date").cloned().unwrap_or_default(),
            "body": truncate_on_char_boundary(text, 100_000),
        }))
        .map_err(|err| err.to_string())
    }

    fn send(&self, message_json: &str) -> Result<String, String> {
        let config = self.config()?;
        let smtp = config
            .smtp
            .as_ref()
            .ok_or_else(|| "email SMTP capability is not configured".to_string())?;
        let password = self.password(config)?;
        let email: OutboundEmail = serde_json::from_str(message_json)
            .map_err(|err| format!("invalid email message JSON: {err}"))?;
        if email.to.is_empty() {
            return Err("email must have at least one recipient".to_string());
        }
        let from = email
            .from
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .or(config.from.as_deref())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(&config.username);
        let mut builder = Message::builder()
            .from(
                from.parse()
                    .map_err(|err| format!("invalid from address: {err}"))?,
            )
            .subject(email.subject)
            .header(ContentType::TEXT_PLAIN);
        for recipient in email.to {
            builder = builder.to(recipient
                .parse()
                .map_err(|err| format!("invalid to address: {err}"))?);
        }
        for recipient in email.cc {
            builder = builder.cc(recipient
                .parse()
                .map_err(|err| format!("invalid cc address: {err}"))?);
        }
        for recipient in email.bcc {
            builder = builder.bcc(
                recipient
                    .parse()
                    .map_err(|err| format!("invalid bcc address: {err}"))?,
            );
        }
        let message = builder
            .body(email.body)
            .map_err(|err| format!("failed to build email: {err}"))?;
        let credentials = Credentials::new(config.username.clone(), password);
        let mailer = match smtp.tls {
            EmailTlsMode::Tls => SmtpTransport::relay(&smtp.host),
            EmailTlsMode::StartTls => SmtpTransport::starttls_relay(&smtp.host),
        }
        .map_err(|err| err.to_string())?
        .port(smtp.port)
        .credentials(credentials)
        .build();
        let response = mailer.send(&message).map_err(|err| err.to_string())?;
        serde_json::to_string(&serde_json::json!({
            "ok": true,
            "response": response.message().collect::<Vec<_>>().join(" "),
        }))
        .map_err(|err| err.to_string())
    }

    fn imap_session(
        &self,
        config: &EmailCapabilityConfig,
        imap: &EmailServerConfig,
    ) -> Result<imap::Session<imap::Connection>, String> {
        let password = self.password(config)?;
        let mode = match imap.tls {
            EmailTlsMode::Tls => imap::ConnectionMode::Tls,
            EmailTlsMode::StartTls => imap::ConnectionMode::StartTls,
        };
        let client = imap::ClientBuilder::new(&imap.host, imap.port)
            .mode(mode)
            .connect()
            .map_err(|err| err.to_string())?;
        client
            .login(&config.username, password)
            .map_err(|(err, _client)| err.to_string())
    }

    fn password(&self, config: &EmailCapabilityConfig) -> Result<String, String> {
        self.secrets
            .get(&config.password_secret)
            .ok_or_else(|| format!("missing email secret: {}", config.password_secret))
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

fn split_message(message: &[u8]) -> (&[u8], String) {
    let split = message
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| (index, 4))
        .or_else(|| {
            message
                .windows(2)
                .position(|window| window == b"\n\n")
                .map(|index| (index, 2))
        });
    let Some((index, separator_len)) = split else {
        return (message, String::new());
    };
    (
        &message[..index],
        String::from_utf8_lossy(&message[index + separator_len..]).to_string(),
    )
}

fn parse_headers(bytes: &[u8]) -> std::collections::BTreeMap<String, String> {
    let text = String::from_utf8_lossy(bytes);
    let mut headers = std::collections::BTreeMap::new();
    let mut current_name: Option<String> = None;
    let mut current_value = String::new();

    for line in text.lines() {
        if line.starts_with(' ') || line.starts_with('\t') {
            if !current_value.is_empty() {
                current_value.push(' ');
            }
            current_value.push_str(line.trim());
            continue;
        }
        if let Some(name) = current_name.take() {
            headers.insert(name, current_value.trim().to_string());
            current_value.clear();
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        current_name = Some(name.trim().to_ascii_lowercase());
        current_value.push_str(value.trim());
    }
    if let Some(name) = current_name {
        headers.insert(name, current_value.trim().to_string());
    }

    headers
}

fn truncate_on_char_boundary(body: String, max: usize) -> String {
    if body.len() <= max {
        return body;
    }
    let mut end = max;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    body[..end].to_string()
}

impl WasiView for StoreData {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}
