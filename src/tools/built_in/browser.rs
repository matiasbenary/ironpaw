use std::{
    collections::HashMap,
    fs,
    hash::{Hash, Hasher},
    path::PathBuf,
    time::Duration,
};

use async_trait::async_trait;
use chromiumoxide::{Browser, BrowserConfig, Page};
use futures::StreamExt;

use crate::{
    automations::{Automation, AutomationEffect, Trigger},
    bus::InvocationScope,
    config::RuntimeLayout,
    message::ToolDefinition,
    tools::{self, ExecutionContext, InternalTool, ToolResponder},
};

mod install;
use crate::network_proxy::ConnectProxy;
use install::ensure_chrome;

pub const UPDATE_AUTOMATION_NAME: &str = "browser-update";

#[derive(Clone, serde::Deserialize, schemars::JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum BrowserParams {
    /// Return the current interactive accessibility-like tree with stable refs.
    Snapshot,
    /// Act on an element ref returned by snapshot. Mirrors browser-agent click, fill, and select actions.
    Act {
        kind: BrowserAct,
        #[serde(rename = "ref")]
        r#ref: String,
        #[serde(default)]
        value: Option<String>,
    },
    /// Equivalent to Playwright page.goto(). Returns the rendered page snapshot.
    Goto { url: String },
}

#[derive(Clone, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BrowserAct {
    Click,
    Fill,
    Select,
}

#[derive(serde::Serialize)]
struct BrowserOutput {
    url: String,
    snapshot: BrowserNode,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct BrowserNode {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    reference: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    href: Option<String>,
    value: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    options: Vec<BrowserOption>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    children: Vec<BrowserNode>,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct BrowserOption {
    name: String,
    value: String,
}

struct Session {
    _browser: Browser,
    _proxy: ConnectProxy,
    page: Page,
    handler: tokio::task::JoinHandle<()>,
    log_path: PathBuf,
    last_url: Option<String>,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.handler.abort();
    }
}

pub struct BrowserTool {
    layout: RuntimeLayout,
    sessions: tokio::sync::Mutex<HashMap<String, Session>>,
}

pub struct BrowserUpdateTool {
    layout: RuntimeLayout,
}

impl BrowserTool {
    pub fn new(layout: RuntimeLayout) -> Self {
        Self {
            layout,
            sessions: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    async fn execute(
        &self,
        params: BrowserParams,
        ctx: &ExecutionContext,
    ) -> Result<BrowserOutput, String> {
        let key = match &ctx.invoked_by {
            InvocationScope::Conversation(key) => format!("conversation:{}", key.storage_key()),
            InvocationScope::ScheduledAutomation { automation_id } => {
                format!("automation:{automation_id}")
            }
        };
        let mut sessions = self.sessions.lock().await;
        if !sessions.contains_key(&key) {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            key.hash(&mut hasher);
            let profile = self
                .layout
                .data_dir()
                .join("browser")
                .join(format!("{:x}", hasher.finish()));
            fs::create_dir_all(&profile).map_err(|err| err.to_string())?;
            sessions.insert(key.clone(), launch_session(&self.layout, profile).await?);
        }
        let result = {
            let session = sessions.get(&key).expect("session inserted");
            execute_session(session, &params).await
        };
        if let Err(error) = &result
            && is_connection_error(error)
        {
            let session = sessions.remove(&key).expect("session exists");
            let log = chrome_log_tail(&session.log_path);
            let log_path = session.log_path.display().to_string();
            let last_url = session.last_url.clone();
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            key.hash(&mut hasher);
            let profile = self
                .layout
                .data_dir()
                .join("browser")
                .join(format!("{:x}", hasher.finish()));
            let replacement = launch_session(&self.layout, profile).await?;
            if let Some(url) = last_url.filter(|url| !url.is_empty() && url != "about:blank") {
                validate_navigation_url(&url)?;
                replacement.page.goto(url).await.map_err(|err| {
                    format!("browser restarted but could not restore its page: {err}")
                })?;
            }
            let retry = matches!(params, BrowserParams::Snapshot | BrowserParams::Goto { .. });
            sessions.insert(key.clone(), replacement);
            if retry {
                let session = sessions.get(&key).expect("replacement inserted");
                return execute_session(session, &params).await;
            }
            return Err(format!(
                "{error}\nBrowser session restarted; call browser({{action:'snapshot'}}) before retrying this action. Chrome log ({log_path}): {log}",
            ));
        }
        if let Ok(output) = &result {
            sessions.get_mut(&key).expect("session exists").last_url = Some(output.url.clone());
        }
        result
    }
}

impl BrowserUpdateTool {
    pub fn new(layout: RuntimeLayout) -> Self {
        Self { layout }
    }

    pub fn automation() -> Automation {
        Automation {
            name: UPDATE_AUTOMATION_NAME.to_string(),
            trigger: Trigger::cron("0 0 * * * * *").expect("valid hourly cron"),
            effect: AutomationEffect::Tool {
                tool: "browser_update".to_string(),
                args: serde_json::json!({}),
            },
        }
    }
}

async fn launch_session(layout: &RuntimeLayout, profile: PathBuf) -> Result<Session, String> {
    let chrome = ensure_chrome(layout.clone()).await?;
    let proxy = ConnectProxy::public_tcp().await?;
    let log_path = profile.join("chrome.log");
    let config = BrowserConfig::builder()
        .chrome_executable(chrome)
        .user_data_dir(profile)
        .arg(format!("--proxy-server=http://{}", proxy.tcp_address()))
        .arg("--proxy-bypass-list=<-loopback>")
        .arg("--disable-quic")
        .arg("--dns-prefetch-disable")
        .arg("--force-webrtc-ip-handling-policy=disable_non_proxied_udp")
        .arg("--enable-logging")
        .arg(format!("--log-file={}", log_path.display()))
        .build()
        .map_err(|err| err.to_string())?;
    let (browser, mut handler) = Browser::launch(config)
        .await
        .map_err(|err| format!("failed to launch managed Headless Shell: {err}"))?;
    let handler = tokio::spawn(async move { while handler.next().await.is_some() {} });
    let page = browser
        .new_page("about:blank")
        .await
        .map_err(|err| err.to_string())?;
    Ok(Session {
        _browser: browser,
        _proxy: proxy,
        page,
        handler,
        log_path,
        last_url: None,
    })
}

async fn execute_session(
    session: &Session,
    params: &BrowserParams,
) -> Result<BrowserOutput, String> {
    match params {
        BrowserParams::Snapshot => snapshot(&session.page).await,
        BrowserParams::Act { kind, r#ref, value } => {
            act(&session.page, kind.clone(), r#ref, value.as_deref()).await
        }
        BrowserParams::Goto { url } => {
            validate_navigation_url(url)?;
            session
                .page
                .goto(url)
                .await
                .map_err(|err| err.to_string())?;
            snapshot(&session.page).await
        }
    }
}

fn validate_navigation_url(url: &str) -> Result<(), String> {
    let url = reqwest::Url::parse(url).map_err(|err| format!("invalid browser url: {err}"))?;
    if url.scheme() != "https" || url.host_str().is_none() {
        return Err("browser navigation requires a public https url".to_string());
    }
    Ok(())
}

fn is_connection_error(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("connection closed")
        || error.contains("channel closed")
        || error.contains("websocket")
        || error.contains("broken pipe")
}

fn chrome_log_tail(path: &PathBuf) -> String {
    let Ok(log) = fs::read_to_string(path) else {
        return format!("unavailable ({})", path.display());
    };
    let start = log
        .char_indices()
        .rev()
        .nth(4_000)
        .map_or(0, |(index, _)| index);
    log[start..].trim().to_string()
}

async fn snapshot(page: &Page) -> Result<BrowserOutput, String> {
    Ok(BrowserOutput {
        url: page
            .url()
            .await
            .map_err(|err| err.to_string())?
            .unwrap_or_default(),
        snapshot: snapshot_tree(page).await?,
    })
}

async fn snapshot_tree(page: &Page) -> Result<BrowserNode, String> {
    let script = r#"(() => {
      let next = 1;
      let count = 0;
      for (const element of document.querySelectorAll('[data-ironpaw-ref]')) element.removeAttribute('data-ironpaw-ref');
      const visible = (element) => {
        const style = getComputedStyle(element);
        return style.display !== 'none' && style.visibility !== 'hidden' && !!element.getClientRects().length;
      };
      const directText = (element) => Array.from(element.childNodes)
        .filter((node) => node.nodeType === Node.TEXT_NODE)
        .map((node) => node.textContent.trim())
        .filter(Boolean)
        .join(' ')
        .slice(0, 500);
      const role = (element) => element.getAttribute('role') || ({
        A: 'link', BUTTON: 'button', SELECT: 'combobox', TEXTAREA: 'textbox',
        INPUT: element.type === 'checkbox' ? 'checkbox' : element.type === 'radio' ? 'radio' : 'textbox',
        MAIN: 'main', NAV: 'navigation', HEADER: 'banner', FOOTER: 'contentinfo',
        SECTION: 'section', ARTICLE: 'article', ASIDE: 'complementary', FORM: 'form',
        FIELDSET: 'group', UL: 'list', OL: 'list', LI: 'listitem',
        TABLE: 'table', TR: 'row', TH: 'columnheader', TD: 'cell',
        H1: 'heading', H2: 'heading', H3: 'heading', H4: 'heading', H5: 'heading', H6: 'heading'
      })[element.tagName] || null;
      const interactive = (element, nodeRole) => ['link', 'button', 'textbox', 'combobox', 'checkbox', 'radio'].includes(nodeRole);
      const name = (element, nodeRole) => {
        const value = element.getAttribute('aria-label') || element.labels?.[0]?.innerText || element.alt || directText(element) || (interactive(element, nodeRole) ? element.innerText || element.placeholder || element.name || element.id : '');
        return value.trim().slice(0, 500) || null;
      };
      const visit = (element) => {
        if (count >= 400 || !visible(element) || element.matches('script,style,template,noscript')) return [];
        const nodeRole = role(element);
        const children = Array.from(element.children).flatMap(visit);
        const text = directText(element);
        if (!nodeRole) {
          return [...(text ? [{ role: 'text', name: text, value: null, options: [], children: [] }] : []), ...children];
        }
        count += 1;
        const isInteractive = interactive(element, nodeRole);
        const reference = isInteractive ? `e${next++}` : null;
        if (reference) element.setAttribute('data-ironpaw-ref', reference);
        const options = element.tagName === 'SELECT'
          ? Array.from(element.options).slice(0, 100).map((option) => ({ name: option.text.trim(), value: option.value }))
          : [];
        return [{
          role: nodeRole,
          name: name(element, nodeRole),
          ref: reference,
          href: element.tagName === 'A' && element.href ? element.href : null,
          value: ['INPUT', 'TEXTAREA', 'SELECT'].includes(element.tagName) ? element.value || null : null,
          options,
          children: [...(text ? [{ role: 'text', name: text, value: null, options: [], children: [] }] : []), ...children]
        }];
      };
      return { role: 'document', value: null, options: [], children: visit(document.body) };
    })()"#;
    page.evaluate_expression(script)
        .await
        .map_err(|err| err.to_string())?
        .into_value()
        .map_err(|err| format!("invalid browser snapshot: {err}"))
}

async fn act(
    page: &Page,
    kind: BrowserAct,
    reference: &str,
    value: Option<&str>,
) -> Result<BrowserOutput, String> {
    if !reference.starts_with('e') || reference[1..].parse::<usize>().is_err() {
        return Err("reference must come from the most recent browser snapshot".to_string());
    }
    let selector = serde_json::to_string(&format!(r#"[data-ironpaw-ref="{reference}"]"#))
        .map_err(|err| err.to_string())?;
    match kind {
        BrowserAct::Click => {
            evaluate_action(page, format!("(() => {{ const e = document.querySelector({selector}); if (!e) throw new Error('stale reference'); e.click(); }})()"))
                .await?;
        }
        BrowserAct::Fill => {
            let value = value.ok_or_else(|| "fill requires value".to_string())?;
            let value = serde_json::to_string(value).map_err(|err| err.to_string())?;
            evaluate_action(page, format!("(() => {{ const e = document.querySelector({selector}); if (!e) throw new Error('stale reference'); e.focus(); e.value = {value}; e.dispatchEvent(new Event('input', {{bubbles:true}})); e.dispatchEvent(new Event('change', {{bubbles:true}})); }})()"))
                .await?;
        }
        BrowserAct::Select => {
            let value = value.ok_or_else(|| "select requires value".to_string())?;
            let value = serde_json::to_string(value).map_err(|err| err.to_string())?;
            evaluate_action(page, format!("(() => {{ const e = document.querySelector({selector}); if (!e) throw new Error('stale reference'); e.value = {value}; e.dispatchEvent(new Event('input', {{bubbles:true}})); e.dispatchEvent(new Event('change', {{bubbles:true}})); }})()"))
                .await?;
        }
    }
    tokio::time::sleep(Duration::from_millis(250)).await;
    snapshot(page).await
}

async fn evaluate_action(page: &Page, script: String) -> Result<(), String> {
    page.evaluate_expression(script).await.map_err(|err| {
        let message = err.to_string();
        if message.contains("stale reference") {
            "browser reference is stale; call browser({action:'snapshot'}) and retry".to_string()
        } else {
            message
        }
    })?;
    Ok(())
}
#[async_trait]
impl InternalTool for BrowserTool {
    fn name(&self) -> &str {
        "browser"
    }
    fn tool_definition(&self) -> ToolDefinition {
        tools::tool_definition::<BrowserParams>(
            self.name(),
            "Navigate a website and interact with its HTML components. Use goto \
             to open a page, snapshot to inspect it, then act with refs returned \
             by the latest snapshot.",
        )
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ExecutionContext,
        responder: ToolResponder,
    ) {
        let result = async {
            let params = serde_json::from_value(args).map_err(|err| err.to_string())?;
            let output = self.execute(params, ctx).await?;
            serde_json::to_value(output).map_err(|err| err.to_string())
        }
        .await;
        responder.respond(result.map_err(crate::tools::ToolError::new));
    }
}

#[async_trait]
impl InternalTool for BrowserUpdateTool {
    fn name(&self) -> &str {
        "browser_update"
    }

    fn tool_definition(&self) -> ToolDefinition {
        tools::tool_definition::<serde_json::Value>(
            self.name(),
            "Internal maintenance tool that updates Ironpaw's managed Chrome runtime.",
        )
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ExecutionContext,
        responder: ToolResponder,
    ) {
        let result = ensure_chrome(self.layout.clone())
            .await
            .map(|path| serde_json::json!({ "path": path }))
            .map_err(crate::tools::ToolError::new);
        responder.respond(result);
    }
}
