use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

type Callback = Result<String, String>;
type CallbackState = Arc<Mutex<Option<Callback>>>;
static CALLBACKS: OnceLock<Mutex<BTreeMap<String, CallbackState>>> = OnceLock::new();

#[derive(Clone)]
pub(crate) struct ProviderSettings {
    pub(crate) authorize_url: &'static str,
    pub(crate) token_url: &'static str,
    pub(crate) client_id: &'static str,
    pub(crate) client_secret: Option<&'static str>,
    pub(crate) redirect_host: &'static str,
    pub(crate) refresh_token_secret: &'static str,
    pub(crate) scopes: Vec<&'static str>,
    pub(crate) extra_authorize_params: Vec<(&'static str, &'static str)>,
}

#[derive(Clone)]
pub(crate) struct OauthFlow {
    pub authorization_url: String,
    settings: ProviderSettings,
    redirect_uri: String,
    state: String,
    code_verifier: String,
}

impl OauthFlow {
    pub(crate) fn refresh_token_secret(&self) -> &'static str {
        self.settings.refresh_token_secret
    }
}

pub(crate) async fn start(
    settings: ProviderSettings,
    login_hint: &str,
) -> Result<OauthFlow, String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| e.to_string())?;
    let redirect_uri = format!(
        "http://{}:{}",
        settings.redirect_host,
        listener.local_addr().map_err(|e| e.to_string())?.port()
    );
    let state = random_urlsafe(32);
    let code_verifier = random_urlsafe(48);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()));
    let mut url = reqwest::Url::parse(settings.authorize_url).expect("valid OAuth authorize URL");
    url.query_pairs_mut()
        .append_pair("client_id", settings.client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", &settings.scopes.join(" "))
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("login_hint", login_hint);
    for (key, value) in &settings.extra_authorize_params {
        url.query_pairs_mut().append_pair(key, value);
    }
    let callback = Arc::new(Mutex::new(None));
    callbacks()
        .lock()
        .expect("OAuth callback lock")
        .insert(state.clone(), Arc::clone(&callback));
    let expected_state = state.clone();
    tokio::spawn(async move {
        *callback.lock().expect("OAuth callback state lock") =
            Some(receive_callback(listener, &expected_state).await);
    });
    Ok(OauthFlow {
        authorization_url: url.to_string(),
        settings,
        redirect_uri,
        state,
        code_verifier,
    })
}

pub(crate) async fn finish(flow: &OauthFlow) -> Result<Option<String>, String> {
    let callback = callbacks()
        .lock()
        .expect("OAuth callback lock")
        .get(&flow.state)
        .cloned()
        .ok_or_else(|| "authorization session expired; configure the tool again".to_string())?;
    let code = match callback.lock().expect("OAuth callback state lock").clone() {
        Some(result) => result?,
        None => return Ok(None),
    };
    let settings = &flow.settings;
    let mut form = vec![
        ("client_id", settings.client_id.to_string()),
        ("code", code),
        ("code_verifier", flow.code_verifier.clone()),
        ("grant_type", "authorization_code".to_string()),
        ("redirect_uri", flow.redirect_uri.clone()),
    ];
    if let Some(client_secret) = settings.client_secret {
        form.push(("client_secret", client_secret.to_string()));
    }
    let response = reqwest::Client::new()
        .post(settings.token_url)
        .form(&form)
        .send()
        .await
        .map_err(oauth_error)?;
    let status = response.status();
    let body = response.text().await.map_err(oauth_error)?;
    if !status.is_success() {
        return Err(format!("token exchange failed ({status}): {body}"));
    }
    callbacks()
        .lock()
        .expect("OAuth callback lock")
        .remove(&flow.state);
    let response: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("invalid OAuth token response: {e}"))?;
    response["refresh_token"]
        .as_str()
        .map(str::to_string)
        .map(Some)
        .ok_or_else(|| {
            "the provider returned no refresh token; revoke Ironpaw access and authorize again"
                .to_string()
        })
}

pub(crate) fn callback_received(state: &str) -> bool {
    callbacks()
        .lock()
        .expect("OAuth callback lock")
        .get(state)
        .is_some_and(|callback| {
            callback
                .lock()
                .expect("OAuth callback state lock")
                .is_some()
        })
}

pub(crate) fn authenticated_client(
    settings: ProviderSettings,
    refresh_token: &str,
) -> Result<reqwest::blocking::Client, String> {
    let mut form = vec![
        ("client_id", settings.client_id),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
    ];
    if let Some(client_secret) = settings.client_secret {
        form.push(("client_secret", client_secret));
    }
    let bare = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(oauth_error)?;
    let response = bare
        .post(settings.token_url)
        .form(&form)
        .send()
        .map_err(oauth_error)?;
    let status = response.status();
    let body = response.text().map_err(oauth_error)?;
    if !status.is_success() {
        return Err(format!("token refresh failed ({status}): {body}"));
    }
    let token: serde_json::Value = serde_json::from_str(&body)
        .map_err(|error| format!("invalid OAuth token response: {error}"))?;
    let access_token = token["access_token"]
        .as_str()
        .ok_or_else(|| "provider returned no access token".to_string())?;
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .default_headers({
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {access_token}")
                    .parse()
                    .map_err(|error| format!("invalid OAuth token: {error}"))?,
            );
            headers
        })
        .build()
        .map_err(oauth_error)
}

async fn receive_callback(listener: TcpListener, expected_state: &str) -> Callback {
    let (mut stream, _) = tokio::time::timeout(Duration::from_secs(300), listener.accept())
        .await
        .map_err(|_| "authorization timed out".to_string())?
        .map_err(|e| e.to_string())?;
    let mut request = vec![0; 8192];
    let count = stream.read(&mut request).await.map_err(|e| e.to_string())?;
    let first = String::from_utf8_lossy(&request[..count])
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    let target = first
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| "invalid OAuth callback".to_string())?;
    let url =
        reqwest::Url::parse(&format!("http://127.0.0.1{target}")).map_err(|e| e.to_string())?;
    let query = url.query_pairs().collect::<BTreeMap<_, _>>();
    if query.get("state").map(|v| v.as_ref()) != Some(expected_state) {
        return Err("invalid OAuth state".to_string());
    }
    let result = query.get("code").map(|v| v.to_string()).ok_or_else(|| {
        query
            .get("error_description")
            .or_else(|| query.get("error"))
            .map(|v| format!("provider denied access: {v}"))
            .unwrap_or_else(|| "provider returned no authorization code".to_string())
    });
    let body = if result.is_ok() {
        "<!doctype html><title>Account connected</title><p>Account connected. You can close this window.</p>"
    } else {
        "<!doctype html><title>Connection failed</title><p>Account connection failed. Return to Ironpaw for details.</p>"
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    result
}

fn callbacks() -> &'static Mutex<BTreeMap<String, CallbackState>> {
    CALLBACKS.get_or_init(|| Mutex::new(BTreeMap::new()))
}
fn random_urlsafe(bytes: usize) -> String {
    let mut value = vec![0; bytes];
    rand::thread_rng().fill_bytes(&mut value);
    URL_SAFE_NO_PAD.encode(value)
}
fn oauth_error(error: reqwest::Error) -> String {
    format!("authorization failed: {}", error.without_url())
}
