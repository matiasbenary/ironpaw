use std::{
    collections::BTreeMap,
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
    message::ToolDefinition,
    network::email::protocol,
    network::http::NetworkGate,
    secrets::ToolSecrets,
    tools::{ExecutionContext, InternalTool, ToolError, ToolResult},
};

pub use crate::network::http::NetworkGrantConfig;

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

    async fn execute(&self, args: serde_json::Value, ctx: &ExecutionContext) -> ToolResult {
        let result: Result<serde_json::Value, String> = async {
            let args_json = serde_json::to_string(&args).map_err(|err| err.to_string())?;
            let wasm_path = self.wasm_path.clone();
            let secrets = self.secrets.clone();
            let config = self.config.clone();
            let guest_config_json = self.guest_config_json.clone();
            let conversation = ctx.conversation.clone();
            let output = tokio::task::spawn_blocking(move || {
                let mut loaded = LoadedTool::load(&wasm_path, secrets, config)?;
                loaded.set_conversation(conversation);
                loaded.execute(&args_json, &guest_config_json)
            })
            .await
            .map_err(|err| err.to_string())??;
            Ok(output)
        }
        .await;
        result.map_err(ToolError::new)
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
    /// When set, every non-GET request the tool makes needs the user's approval unless its
    /// path is listed in `safe_paths`.
    #[serde(default)]
    pub approval: Option<ApprovalConfig>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct ApprovalConfig {
    #[serde(default)]
    pub safe_paths: Vec<String>,
    /// Extra paths that skip approval when the request comes from one local client, keyed by
    /// client id (e.g. `esp32`). Opt-in per installation, for devices where a card is impractical.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub client_safe_paths: std::collections::BTreeMap<String, Vec<String>>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct EmailCapabilityConfig {
    #[serde(default)]
    pub accounts: BTreeMap<String, EmailAccountCapabilityConfig>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct EmailAccountCapabilityConfig {
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
                approval: tool_config.approval,
                conversation: None,
            },
        );
        let bindings =
            Tool::instantiate(&mut store, &component, &linker).map_err(|err| err.to_string())?;

        Ok(Self { store, bindings })
    }

    fn set_conversation(&mut self, conversation: Option<crate::bus::ConversationKey>) {
        self.store.data_mut().conversation = conversation;
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
    approval: Option<ApprovalConfig>,
    conversation: Option<crate::bus::ConversationKey>,
}

impl StoreData {
    /// `Err` holds what the tool should report instead of sending the request.
    fn require_approval(
        &self,
        account: Option<&str>,
        method: &str,
        url: &str,
        body_json: &str,
    ) -> Result<(), String> {
        let Some(approval) = &self.approval else {
            return Ok(());
        };
        let path = reqwest::Url::parse(url)
            .map(|url| url.path().to_string())
            .map_err(|err| format!("invalid url: {err}"))?;
        let client_safe = self
            .conversation
            .as_ref()
            .filter(|conversation| conversation.channel == crate::channels::local_client::CHANNEL)
            .and_then(|conversation| conversation.thread_id.as_ref())
            .and_then(|client| approval.client_safe_paths.get(client))
            .is_some_and(|paths| paths.contains(&path));
        if method.eq_ignore_ascii_case("GET") || approval.safe_paths.contains(&path) || client_safe
        {
            return Ok(());
        }
        let body = serde_json::from_str::<serde_json::Value>(body_json)
            .map(|body| body.to_string())
            .unwrap_or_else(|_| body_json.to_string());
        let wallet = account.unwrap_or("default");
        let fingerprint = format!("{wallet}|{method}|{url}|{body}");
        let summary = format!(
            "Approve this action?\nWallet: {wallet}\n{} {url}\n{body}",
            method.to_ascii_uppercase()
        );
        crate::tools::approval::Approvals::global().require(
            self.conversation.as_ref(),
            &fingerprint,
            &summary,
        )
    }
}

impl ironpaw::tool::tool_host::Host for StoreData {
    fn http_request(
        &mut self,
        account: Option<String>,
        method: String,
        url: String,
        headers_json: String,
        body_json: String,
    ) -> Result<String, String> {
        self.require_approval(account.as_deref(), &method, &url, &body_json)?;
        self.http
            .request_bytes_headers_for_string_blocking(
                account.as_deref(),
                &method,
                &url,
                &headers_json,
                &body_json,
            )
            .and_then(|bytes| {
                String::from_utf8(bytes).map_err(|err| {
                    crate::network::http::HttpError::Http(format!("response was not UTF-8: {err}"))
                })
            })
            .map_err(|err| err.to_string())
    }

    fn imap_list(
        &mut self,
        account: Option<String>,
        folder: String,
        limit: u32,
    ) -> Result<String, String> {
        self.email.list(account.as_deref(), &folder, limit)
    }

    fn imap_read(
        &mut self,
        account: Option<String>,
        folder: String,
        uid: String,
    ) -> Result<String, String> {
        self.email.read(account.as_deref(), &folder, &uid)
    }

    fn smtp_send(
        &mut self,
        account: Option<String>,
        message_json: String,
    ) -> Result<String, String> {
        self.email.send(account.as_deref(), &message_json)
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

    fn list(&self, account: Option<&str>, folder: &str, limit: u32) -> Result<String, String> {
        let config = self.account(account)?;
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

    fn read(&self, account: Option<&str>, folder: &str, uid: &str) -> Result<String, String> {
        let config = self.account(account)?;
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

    fn send(&self, account: Option<&str>, message_json: &str) -> Result<String, String> {
        let config = self.account(account)?;
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

    fn account(&self, account: Option<&str>) -> Result<&EmailAccountCapabilityConfig, String> {
        let account = account.ok_or_else(|| "email account is required".to_string())?;
        self.config
            .as_ref()
            .ok_or_else(|| "email capability is not configured for this tool".to_string())
            .and_then(|config| {
                config
                    .accounts
                    .get(account)
                    .ok_or_else(|| format!("email account is not configured: {account}"))
            })
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

#[cfg(test)]
mod tests {
    use super::{LoadedTool, WasmToolConfig};

    #[test]
    fn packaged_tools_link_against_the_host() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let secrets = crate::secrets::Secrets::for_tests().tools;
        for (name, directory) in [
            ("email", "email"),
            ("web_search", "web-search"),
            ("outlayer", "outlayer"),
        ] {
            let directory = root.join("wasm-tools").join(directory);
            let config: WasmToolConfig = serde_json::from_str(
                &std::fs::read_to_string(
                    directory.join(format!("{}.config.template.json", name.replace('_', "-"))),
                )
                .unwrap(),
            )
            .unwrap();
            let mut tool = LoadedTool::load(
                directory.join(format!("{}.wasm", name.replace('_', "-"))),
                secrets.clone(),
                config,
            )
            .unwrap();
            assert_eq!(tool.name().unwrap(), name);
        }
    }

    /// Full agent flow through the host against ~/projects/near/outlayer-mock.
    /// Run with: (cd ~/projects/near/outlayer-mock && node server.js) then
    /// cargo test --features local-development outlayer_flow_against_mock -- --ignored
    #[cfg(feature = "local-development")]
    #[test]
    #[ignore = "needs the OutLayer mock on localhost:4400"]
    fn outlayer_flow_against_mock() {
        const MOCK: &str = "http://localhost:4400";
        let admin = |path: &str, body: serde_json::Value| -> serde_json::Value {
            let url = format!("{MOCK}{path}");
            crate::network::client::blocking_for_https_or_loopback(
                &url,
                std::time::Duration::from_secs(10),
            )
            .unwrap()
            .post(url)
                .json(&body)
                .send()
                .and_then(reqwest::blocking::Response::error_for_status)
                .and_then(reqwest::blocking::Response::json)
                .unwrap_or_else(|err| panic!("mock admin {path}: {err}"))
        };
        let wallet = admin(
            "/admin/wallets",
            serde_json::json!({ "label": "ironpaw-test" }),
        );
        let account = wallet["near_account_id"].as_str().unwrap().to_string();
        admin(
            "/admin/wallet",
            serde_json::json!({ "wallet_id": wallet["wallet_id"], "active": true }),
        );
        admin(
            "/admin/balance",
            serde_json::json!({ "account": account, "asset": "near", "source": "wallet", "amount": "5000000000000000000000000" }),
        );

        let directory =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("wasm-tools/outlayer");
        let mut config: WasmToolConfig = serde_json::from_str(
            &std::fs::read_to_string(directory.join("outlayer.config.template.json")).unwrap(),
        )
        .unwrap();
        // The mock's active wallet answers any key; approvals are covered by their own tests.
        config.approval = None;
        config.http_credentials = vec![crate::network::http::NetworkGrantConfig {
            host: "localhost".to_string(),
            account: Some("test".to_string()),
            secret: None,
            placeholder: None,
            header: None,
            prefix: None,
            oauth: None,
        }];
        let mut tool = LoadedTool::load(
            directory.join("outlayer.wasm"),
            crate::secrets::Secrets::for_tests().tools,
            config,
        )
        .unwrap();
        let mut call = |method: &str, path: &str, body: serde_json::Value| {
            tool.execute(
                &serde_json::json!({ "method": method, "path": path, "body": body }).to_string(),
                r#"{"outlayer":{"accounts":{"test":{"network":"mainnet"}}}}"#,
            )
        };
        const NEAR: &str = "1000000000000000000000000";
        let swap = serde_json::json!({
            "token_in": "nep141:wrap.near",
            "token_out": "nep141:usdt.tether-token.near",
            "amount_in": NEAR,
        });

        let address = call(
            "GET",
            "/wallet/v1/address?chain=near",
            serde_json::Value::Null,
        )
        .unwrap();
        assert_eq!(address["address"], account);
        let balance = call(
            "GET",
            "/wallet/v1/balance?chain=near",
            serde_json::Value::Null,
        )
        .unwrap();
        assert_eq!(balance["balance"], "5000000000000000000000000");

        // Swaps spend the intents balance only: nothing deposited yet.
        let err = call("POST", "/wallet/v1/intents/swap", swap.clone()).unwrap_err();
        assert!(err.contains("ntents balance"), "{err}");

        // Native NEAR -> intents: storage, wrap (args JSON-encoded, as models send it), deposit.
        call(
            "POST",
            "/wallet/v1/storage-deposit",
            serde_json::json!({ "token": "wrap.near" }),
        )
        .unwrap();
        let wrap = call(
            "POST",
            "/wallet/v1/call",
            serde_json::json!({ "receiver_id": "wrap.near", "method_name": "near_deposit", "args": "{}",
                "gas": "30000000000000", "deposit": "2000000000000000000000000" }),
        )
        .unwrap();
        assert_eq!(wrap["status"], "success", "{wrap}");
        call(
            "POST",
            "/wallet/v1/intents/deposit",
            serde_json::json!({ "token": "wrap.near", "amount": "2000000000000000000000000" }),
        )
        .unwrap();
        let intents = call(
            "GET",
            "/wallet/v1/balance?token=wrap.near&source=intents",
            serde_json::Value::Null,
        )
        .unwrap();
        assert_eq!(intents["balance"], "2000000000000000000000000");

        let quote = call("POST", "/wallet/v1/intents/swap/quote", swap.clone()).unwrap();
        assert!(
            quote["amount_out"]
                .as_str()
                .unwrap()
                .parse::<u64>()
                .unwrap()
                > 0,
            "{quote}"
        );
        let swapped = call("POST", "/wallet/v1/intents/swap", swap).unwrap();
        assert_eq!(swapped["status"], "success", "{swapped}");

        let withdraw = call(
            "POST",
            "/wallet/v1/intents/withdraw",
            serde_json::json!({ "chain": "near", "to": "bob.near", "amount": "500000000000000000000000", "async": true }),
        )
        .unwrap();
        let id = withdraw["request_id"].as_str().unwrap().to_string();
        admin("/admin/settle", serde_json::json!({}));
        let status = call(
            "GET",
            &format!("/wallet/v1/requests/{id}"),
            serde_json::Value::Null,
        )
        .unwrap();
        assert_eq!(status["status"], "success", "{status}");
    }

    #[test]
    fn money_requests_wait_for_the_user_but_reads_do_not() {
        let mut config: WasmToolConfig = serde_json::from_str(
            r#"{"approval":{"safe_paths":["/wallet/v1/intents/swap/quote"]}}"#,
        )
        .unwrap();
        let mut tool = LoadedTool::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("wasm-tools/outlayer/outlayer.wasm"),
            crate::secrets::Secrets::for_tests().tools,
            config.clone(),
        )
        .unwrap();
        let data = tool.store.data_mut();
        let gate = |data: &super::StoreData, method: &str, path: &str| {
            data.require_approval(Some("w"), method, &format!("https://x.test{path}"), "{}")
        };
        assert!(gate(data, "GET", "/wallet/v1/balance").is_ok());
        assert!(gate(data, "POST", "/wallet/v1/intents/swap/quote").is_ok());
        // No conversation to ask: the money call is refused, not let through.
        assert!(gate(data, "POST", "/wallet/v1/transfer").is_err());
        config.approval = None;
        data.approval = None;
        assert!(gate(data, "POST", "/wallet/v1/transfer").is_ok());
    }

    #[test]
    fn a_client_can_be_trusted_with_specific_paths_only() {
        let config: WasmToolConfig = serde_json::from_str(
            r#"{"approval":{"client_safe_paths":{"esp32":["/wallet/v1/transfer"]}}}"#,
        )
        .unwrap();
        let mut tool = LoadedTool::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("wasm-tools/outlayer/outlayer.wasm"),
            crate::secrets::Secrets::for_tests().tools,
            config,
        )
        .unwrap();
        let from = |tool: &mut LoadedTool, channel: &str, client: &str, path: &str| {
            tool.set_conversation(Some(crate::bus::ConversationKey {
                channel: channel.to_string(),
                chat_id: "chat".to_string(),
                thread_id: Some(client.to_string()),
            }));
            let url = format!("https://x.test{path}");
            // `Ok` only when nothing has to be asked; a held request is an `Err`.
            tool.store.data_mut().require_approval(Some("w"), "POST", &url, "{}")
        };
        assert!(from(&mut tool, "local-client", "esp32", "/wallet/v1/transfer").is_ok());
        assert!(from(&mut tool, "local-client", "esp32", "/wallet/v1/call").is_err());
        assert!(from(&mut tool, "local-client", "webui", "/wallet/v1/transfer").is_err());
        assert!(from(&mut tool, "telegram", "esp32", "/wallet/v1/transfer").is_err());
    }
}
