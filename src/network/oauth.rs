use std::{
    collections::BTreeMap,
    env,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    middleware,
    routing::post,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac as _};
use rand::RngCore;
use sha2::{Digest, Sha256};

type Callback = Result<String, String>;
struct CallbackState {
    result: Option<Callback>,
    expires_at: Instant,
}

static CALLBACKS: OnceLock<Mutex<BTreeMap<String, CallbackState>>> = OnceLock::new();
#[cfg(feature = "local-development")]
const DEFAULT_CALLBACK_URL: &str = "http://localhost:3000/oauth/callback";
#[cfg(not(feature = "local-development"))]
const DEFAULT_CALLBACK_URL: &str = "https://ironpaw.app/oauth/callback";
#[cfg(feature = "local-development")]
const DEFAULT_BROKER_URL: &str = "http://localhost:3000";
#[cfg(not(feature = "local-development"))]
const DEFAULT_BROKER_URL: &str = "https://ironpaw.app";
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct OAuthGrantConfig {
    #[serde(default)]
    pub broker_provider: Option<String>,
    #[serde(default)]
    pub token_url: Option<String>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub client_secret_handle: Option<String>,
    pub refresh_token_handle: String,
}

#[derive(Clone)]
pub(crate) struct ProviderSettings {
    pub(crate) broker_provider: Option<String>,
    pub(crate) authorize_url: Option<String>,
    pub(crate) token_url: Option<String>,
    pub(crate) client_id: Option<String>,
    pub(crate) client_secret: Option<String>,
    pub(crate) refresh_token_handle: String,
    pub(crate) scopes: Vec<String>,
    pub(crate) extra_authorize_params: Vec<(String, String)>,
}

#[derive(Clone)]
pub(crate) struct OauthFlow {
    pub authorization_url: String,
    settings: ProviderSettings,
    redirect_uri: String,
    state: String,
    code_verifier: String,
    binding_key: String,
}

impl OauthFlow {
    pub(crate) fn refresh_token_handle(&self) -> &str {
        &self.settings.refresh_token_handle
    }
}

pub(crate) async fn start(
    settings: ProviderSettings,
    login_hint: &str,
) -> Result<OauthFlow, String> {
    let redirect_uri = callback_url()?;
    let state = random_urlsafe(32);
    let code_verifier = random_urlsafe(48);
    let binding_key = random_urlsafe(32);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()));
    let authorization_url = match &settings.broker_provider {
        Some(provider) => {
            let response =
                super::client::async_for_https_or_loopback(&broker_url()?, Duration::from_secs(60))
                    .await?
                    .post(broker_endpoint("api/oauth/authorize")?)
                    .json(&serde_json::json!({
                        "provider": provider,
                        "redirect_uri": redirect_uri,
                        "state": state,
                        "code_challenge": challenge,
                        "login_hint": login_hint,
                        "scopes": settings.scopes,
                    }))
                    .send()
                    .await
                    .map_err(oauth_error)?;
            broker_response(response).await?["authorization_url"]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| "OAuth broker returned no authorization URL".to_string())?
        }
        None => direct_authorization_url(&settings, login_hint, &redirect_uri, &state, &challenge)?,
    };
    callbacks().lock().expect("OAuth callback lock").insert(
        state.clone(),
        CallbackState {
            result: None,
            expires_at: Instant::now() + CALLBACK_TIMEOUT,
        },
    );
    Ok(OauthFlow {
        authorization_url,
        settings,
        redirect_uri,
        state,
        code_verifier,
        binding_key,
    })
}

pub(crate) async fn finish(flow: &OauthFlow) -> Result<Option<String>, String> {
    let code = {
        let mut callbacks = callbacks().lock().expect("OAuth callback lock");
        let callback = callbacks
            .get(&flow.state)
            .ok_or_else(|| "authorization session expired; configure the tool again".to_string())?;
        if callback.expires_at <= Instant::now() {
            callbacks.remove(&flow.state);
            return Err("authorization timed out".to_string());
        }
        match callback.result.clone() {
            Some(result) => result?,
            None => return Ok(None),
        }
    };
    let credential = match &flow.settings.broker_provider {
        Some(provider) => {
            let response =
                super::client::async_for_https_or_loopback(&broker_url()?, Duration::from_secs(60))
                    .await?
                    .post(broker_endpoint("api/oauth/exchange")?)
                    .json(&serde_json::json!({
                        "provider": provider,
                        "code": code,
                        "code_verifier": flow.code_verifier,
                        "redirect_uri": flow.redirect_uri,
                        "binding_key": flow.binding_key,
                    }))
                    .send()
                    .await
                    .map_err(oauth_error)?;
            let grant = broker_response(response).await?["grant"]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| "OAuth broker returned no grant".to_string())?;
            serde_json::to_string(&BrokerCredential {
                grant,
                binding_key: flow.binding_key.clone(),
            })
            .expect("serialize broker credential")
        }
        None => exchange_direct(flow, code).await?,
    };
    callbacks()
        .lock()
        .expect("OAuth callback lock")
        .remove(&flow.state);
    Ok(Some(credential))
}

#[derive(serde::Deserialize, serde::Serialize)]
struct BrokerCredential {
    grant: String,
    binding_key: String,
}

fn direct_authorization_url(
    settings: &ProviderSettings,
    login_hint: &str,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
) -> Result<String, String> {
    let authorize_url = settings
        .authorize_url
        .as_deref()
        .ok_or_else(|| "OAuth authorization URL is missing".to_string())?;
    let client_id = settings
        .client_id
        .as_deref()
        .ok_or_else(|| "OAuth client id is missing".to_string())?;
    let mut url = reqwest::Url::parse(authorize_url)
        .map_err(|error| format!("invalid OAuth authorization URL: {error}"))?;
    if url.scheme() != "https" || url.host_str().is_none() {
        return Err("OAuth authorization URL must be an absolute HTTPS URL".to_string());
    }
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", &settings.scopes.join(" "))
        .append_pair("state", state)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("login_hint", login_hint);
    for (key, value) in &settings.extra_authorize_params {
        url.query_pairs_mut().append_pair(key, value);
    }
    Ok(url.to_string())
}

async fn exchange_direct(flow: &OauthFlow, code: String) -> Result<String, String> {
    let settings = &flow.settings;
    let token_url = settings
        .token_url
        .as_deref()
        .ok_or_else(|| "OAuth token URL is missing".to_string())?;
    let client_id = settings
        .client_id
        .as_deref()
        .ok_or_else(|| "OAuth client id is missing".to_string())?;
    let mut form = vec![
        ("client_id", client_id.to_string()),
        ("code", code),
        ("code_verifier", flow.code_verifier.clone()),
        ("grant_type", "authorization_code".to_string()),
        ("redirect_uri", flow.redirect_uri.clone()),
    ];
    if let Some(client_secret) = &settings.client_secret {
        form.push(("client_secret", client_secret.clone()));
    }
    let response = super::client::async_for(token_url, Duration::from_secs(60))
        .await?
        .post(token_url)
        .form(&form)
        .send()
        .await
        .map_err(oauth_error)?;
    let token = response_json(response, "token exchange").await?;
    token["refresh_token"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| {
            "the provider returned no refresh token; revoke Ironpaw access and authorize again"
                .to_string()
        })
}

fn refresh_brokered(settings: &OAuthGrantConfig, credential: &str) -> Result<String, String> {
    let credential: BrokerCredential = serde_json::from_str(credential)
        .map_err(|error| format!("invalid OAuth broker credential: {error}"))?;
    let mut mac = Hmac::<Sha256>::new_from_slice(credential.binding_key.as_bytes())
        .expect("HMAC accepts keys of any length");
    mac.update(credential.grant.as_bytes());
    let proof = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
    let response =
        super::client::blocking_for_https_or_loopback(&broker_url()?, Duration::from_secs(60))?
            .post(broker_endpoint("api/oauth/refresh")?)
            .json(&serde_json::json!({
                "provider": settings.broker_provider,
                "grant": credential.grant,
                "proof": proof,
            }))
            .send()
            .map_err(oauth_error)?;
    let token = response_json_blocking(response, "token refresh")?;
    token["access_token"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "OAuth broker returned no access token".to_string())
}

fn broker_url() -> Result<String, String> {
    let value =
        env::var("IRONPAW_OAUTH_BROKER_URL").unwrap_or_else(|_| DEFAULT_BROKER_URL.to_string());
    validate_service_url(&value, "OAuth broker")
}

fn broker_endpoint(path: &str) -> Result<reqwest::Url, String> {
    reqwest::Url::parse(&format!("{}/{path}", broker_url()?.trim_end_matches('/')))
        .map_err(|error| format!("invalid OAuth broker URL: {error}"))
}

fn validate_service_url(value: &str, name: &str) -> Result<String, String> {
    let url = reqwest::Url::parse(value).map_err(|error| format!("invalid {name} URL: {error}"))?;
    let local_http = cfg!(feature = "local-development")
        && url.scheme() == "http"
        && matches!(url.host_str(), Some("localhost" | "127.0.0.1"));
    if url.host_str().is_none() || (url.scheme() != "https" && !local_http) {
        return Err(format!("{name} URL must use HTTPS"));
    }
    Ok(url.to_string().trim_end_matches('/').to_string())
}

async fn broker_response(response: reqwest::Response) -> Result<serde_json::Value, String> {
    response_json(response, "OAuth broker request").await
}

async fn response_json(
    response: reqwest::Response,
    operation: &str,
) -> Result<serde_json::Value, String> {
    let status = response.status();
    let body = response.text().await.map_err(oauth_error)?;
    if !status.is_success() {
        return Err(format!("{operation} failed ({status}): {body}"));
    }
    serde_json::from_str(&body).map_err(|error| format!("invalid {operation} response: {error}"))
}

fn response_json_blocking(
    response: reqwest::blocking::Response,
    operation: &str,
) -> Result<serde_json::Value, String> {
    let status = response.status();
    let body = response.text().map_err(oauth_error)?;
    if !status.is_success() {
        return Err(format!("{operation} failed ({status}): {body}"));
    }
    serde_json::from_str(&body).map_err(|error| format!("invalid {operation} response: {error}"))
}

fn complete_callback(state: &str, result: Callback) -> Result<(), String> {
    let mut callbacks = callbacks().lock().expect("OAuth callback lock");
    let callback = callbacks
        .get_mut(state)
        .ok_or_else(|| "unknown or expired OAuth state".to_string())?;
    if callback.expires_at <= Instant::now() {
        callbacks.remove(state);
        return Err("authorization timed out".to_string());
    }
    if callback.result.is_some() {
        return Err("OAuth callback was already received".to_string());
    }
    callback.result = Some(result);
    Ok(())
}

pub(crate) fn refresh_access_token(
    settings: &OAuthGrantConfig,
    credential: &str,
    client_secret: Option<&str>,
) -> Result<String, String> {
    if settings.broker_provider.is_some() {
        return refresh_brokered(settings, credential);
    }
    let token_url = settings
        .token_url
        .as_deref()
        .ok_or_else(|| "OAuth token URL is missing".to_string())?;
    let client_id = settings
        .client_id
        .as_deref()
        .ok_or_else(|| "OAuth client id is missing".to_string())?;
    let mut form = vec![
        ("client_id", client_id.to_string()),
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", credential.to_string()),
    ];
    if let Some(secret) = client_secret {
        form.push(("client_secret", secret.to_string()));
    }
    let response = super::client::blocking_for(token_url, Duration::from_secs(60))?
        .post(token_url)
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
    token["access_token"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "provider returned no access token".to_string())
}

fn callbacks() -> &'static Mutex<BTreeMap<String, CallbackState>> {
    CALLBACKS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn callback_url() -> Result<String, String> {
    let value =
        env::var("IRONPAW_OAUTH_CALLBACK_URL").unwrap_or_else(|_| DEFAULT_CALLBACK_URL.to_string());
    validate_callback_url(&value)
}

fn validate_callback_url(value: &str) -> Result<String, String> {
    let value = validate_service_url(value, "OAuth callback")?;
    let url = reqwest::Url::parse(&value).expect("validated callback URL");
    if url.query().is_some() || url.fragment().is_some() {
        return Err("OAuth callback URL must not contain a query or fragment".to_string());
    }
    Ok(url.to_string())
}

#[derive(Clone)]
struct RouteState {
    token: String,
}

#[derive(serde::Deserialize)]
struct CallbackRequest {
    state: String,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

pub(super) fn routes(token: String) -> Router {
    Router::new()
        .route(
            "/oauth/callback",
            post(receive_callback).options(super::server::preflight),
        )
        .layer(middleware::from_fn(super::server::cors))
        .with_state(RouteState { token })
}

async fn receive_callback(
    State(route): State<RouteState>,
    headers: HeaderMap,
    Json(request): Json<CallbackRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    super::server::auth::authorize_bearer(&headers, &route.token)?;
    let result = request.code.ok_or_else(|| {
        request
            .error_description
            .or(request.error)
            .map(|error| format!("provider denied access: {error}"))
            .unwrap_or_else(|| "provider returned no authorization code".to_string())
    });
    complete_callback(&request.state, result).map_err(|error| (StatusCode::BAD_REQUEST, error))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

fn random_urlsafe(bytes: usize) -> String {
    let mut value = vec![0; bytes];
    rand::thread_rng().fill_bytes(&mut value);
    URL_SAFE_NO_PAD.encode(value)
}
fn oauth_error(error: reqwest::Error) -> String {
    format!("authorization failed: {}", error.without_url())
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::{CallbackState, callbacks, complete_callback, validate_callback_url};

    #[test]
    fn callback_http_matches_local_development_feature() {
        assert_eq!(
            validate_callback_url("http://localhost:3000/oauth/callback").is_ok(),
            cfg!(feature = "local-development")
        );
        assert!(validate_callback_url("https://ironpaw.app/oauth/callback").is_ok());
    }

    #[test]
    fn callback_rejects_public_http_and_query_parameters() {
        assert!(validate_callback_url("http://ironpaw.app/oauth/callback").is_err());
        assert!(validate_callback_url("https://ironpaw.app/oauth/callback?code=x").is_err());
    }

    #[test]
    fn callback_is_accepted_once() {
        let state = uuid::Uuid::new_v4().to_string();
        callbacks().lock().unwrap().insert(
            state.clone(),
            CallbackState {
                result: None,
                expires_at: Instant::now() + super::CALLBACK_TIMEOUT,
            },
        );

        assert!(complete_callback(&state, Ok("code".to_string())).is_ok());
        assert!(complete_callback(&state, Ok("other".to_string())).is_err());

        callbacks().lock().unwrap().remove(&state);
    }
}
