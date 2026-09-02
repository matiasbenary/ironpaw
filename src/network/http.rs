use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::{
    Method,
    blocking::{Client, RequestBuilder, multipart},
};

pub use super::oauth::OAuthGrantConfig;

pub trait SecretLookup: Clone + Send + Sync + 'static {
    fn get_secret(&self, name: &str) -> Option<String>;
}

#[derive(Clone, Debug)]
pub struct NetworkGrant {
    pub host: String,
    pub account: Option<String>,
    pub secret: Option<String>,
    pub header: String,
    pub prefix: String,
    pub placeholder: Option<String>,
    pub oauth: Option<OAuthGrantConfig>,
}

impl NetworkGrant {
    pub fn bearer(host: impl Into<String>, secret: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            account: None,
            secret: Some(secret.into()),
            header: "Authorization".to_string(),
            prefix: "Bearer ".to_string(),
            placeholder: None,
            oauth: None,
        }
    }

    pub fn url_placeholder(
        host: impl Into<String>,
        placeholder: impl Into<String>,
        secret: impl Into<String>,
    ) -> Self {
        Self {
            host: host.into(),
            account: None,
            secret: Some(secret.into()),
            header: String::new(),
            prefix: String::new(),
            placeholder: Some(placeholder.into()),
            oauth: None,
        }
    }
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct NetworkGrantConfig {
    pub host: String,
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default)]
    pub placeholder: Option<String>,
    #[serde(default)]
    pub header: Option<String>,
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub oauth: Option<OAuthGrantConfig>,
}

impl From<NetworkGrantConfig> for NetworkGrant {
    fn from(config: NetworkGrantConfig) -> Self {
        if let Some(placeholder) = config.placeholder {
            return Self {
                host: config.host,
                account: config.account,
                secret: config.secret,
                header: String::new(),
                prefix: String::new(),
                placeholder: Some(placeholder),
                oauth: config.oauth,
            };
        }
        Self {
            host: config.host,
            account: config.account,
            secret: config.secret,
            header: config.header.unwrap_or_else(|| "Authorization".to_string()),
            prefix: config.prefix.unwrap_or_else(|| "Bearer ".to_string()),
            placeholder: None,
            oauth: config.oauth,
        }
    }
}

#[derive(Debug)]
pub enum HttpError {
    InvalidRequest(String),
    MissingCredential(String),
    Http(String),
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(message) => write!(f, "invalid request: {message}"),
            Self::MissingCredential(name) => write!(f, "missing credential: {name}"),
            Self::Http(message) => write!(f, "http error: {message}"),
        }
    }
}

impl std::error::Error for HttpError {}

#[derive(Clone)]
pub struct NetworkGate<S> {
    secrets: S,
    grants: Vec<NetworkGrant>,
    oauth_tokens: Arc<Mutex<BTreeMap<String, String>>>,
    timeout: Duration,
}

pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

impl<S> NetworkGate<S>
where
    S: SecretLookup,
{
    pub fn new(secrets: S, grants: Vec<NetworkGrant>) -> Self {
        Self {
            secrets,
            grants,
            oauth_tokens: Arc::new(Mutex::new(BTreeMap::new())),
            timeout: Duration::from_secs(60),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn post_json_blocking<T>(&self, url: &str, body: &T) -> Result<String, HttpError>
    where
        T: serde::Serialize + ?Sized,
    {
        let url = self.prepare_url(url)?;
        self.send_text(self.client(&url)?.post(url.clone()).json(body), &url)
    }

    pub async fn post_json<T>(&self, url: &str, body: &T) -> Result<HttpResponse, HttpError>
    where
        T: serde::Serialize + ?Sized,
    {
        let url = self.prepare_url(url)?;
        let response = self
            .authorize_async(
                self.async_client(&url).await?.post(url.clone()).json(body),
                &url,
            )?
            .send()
            .await
            .map_err(sanitize_reqwest_error)?;
        let status = response.status().as_u16();
        let body = response.text().await.map_err(sanitize_reqwest_error)?;
        Ok(HttpResponse { status, body })
    }

    pub fn post_json_string_blocking(
        &self,
        url: &str,
        body_json: &str,
    ) -> Result<String, HttpError> {
        let url = self.prepare_url(url)?;
        let body_json = self.inject_string_credentials(body_json, &url)?;
        let body = serde_json::from_str::<serde_json::Value>(&body_json)
            .map_err(|err| HttpError::InvalidRequest(format!("invalid JSON body: {err}")))?;
        self.send_text(self.client(&url)?.post(url.clone()).json(&body), &url)
    }

    pub fn request_bytes_string_blocking(
        &self,
        method: &str,
        url: &str,
        body_json: &str,
    ) -> Result<Vec<u8>, HttpError> {
        self.request_bytes_headers_string_blocking(method, url, "{}", body_json)
    }

    pub fn request_bytes_headers_string_blocking(
        &self,
        method: &str,
        url: &str,
        headers_json: &str,
        body_json: &str,
    ) -> Result<Vec<u8>, HttpError> {
        self.request_bytes_headers_for_string_blocking(None, method, url, headers_json, body_json)
    }

    pub fn request_bytes_headers_for_string_blocking(
        &self,
        account: Option<&str>,
        method: &str,
        url: &str,
        headers_json: &str,
        body_json: &str,
    ) -> Result<Vec<u8>, HttpError> {
        let method = method
            .parse::<Method>()
            .map_err(|err| HttpError::InvalidRequest(format!("invalid HTTP method: {err}")))?;
        if !matches!(
            method,
            Method::GET | Method::POST | Method::PATCH | Method::DELETE
        ) {
            return Err(HttpError::InvalidRequest(
                "only GET, POST, PATCH, and DELETE are supported".to_string(),
            ));
        }

        let url = self.prepare_url(url)?;
        let mut request = self.client(&url)?.request(method.clone(), url.clone());
        let headers: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(headers_json)
                .map_err(|err| HttpError::InvalidRequest(format!("invalid JSON headers: {err}")))?;
        for (name, value) in headers {
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "authorization" | "cookie" | "host" | "proxy-authorization"
            ) {
                return Err(HttpError::InvalidRequest(format!(
                    "guest may not set protected header: {name}"
                )));
            }
            let value = value.as_str().ok_or_else(|| {
                HttpError::InvalidRequest(format!("header {name} must be a string"))
            })?;
            request = request.header(name, value);
        }
        if method != Method::GET && !body_json.trim().is_empty() {
            let body_json = self.inject_string_credentials(body_json, &url)?;
            let body = serde_json::from_str::<serde_json::Value>(&body_json)
                .map_err(|err| HttpError::InvalidRequest(format!("invalid JSON body: {err}")))?;
            request = request.json(&body);
        }

        let response = self
            .authorize_for(request, &url, account)?
            .send()
            .map_err(sanitize_blocking_reqwest_error)?;
        let status = response.status();
        let body = response.bytes().map_err(sanitize_blocking_reqwest_error)?;

        if !status.is_success() {
            let body = String::from_utf8_lossy(&body);
            return Err(HttpError::Http(format!(
                "request failed with {status}: {body}"
            )));
        }

        Ok(body.to_vec())
    }

    pub fn post_multipart_base64_blocking(
        &self,
        url: &str,
        fields_json: &str,
        file_field: &str,
        filename: &str,
        mime_type: &str,
        data_base64: &str,
    ) -> Result<String, HttpError> {
        let url = self.prepare_url(url)?;
        let fields_json = self.inject_string_credentials(fields_json, &url)?;
        let fields =
            serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&fields_json)
                .map_err(|err| HttpError::InvalidRequest(format!("invalid JSON fields: {err}")))?;
        let file_bytes = STANDARD
            .decode(data_base64)
            .map_err(|err| HttpError::InvalidRequest(format!("invalid base64 data: {err}")))?;
        let file_part = multipart::Part::bytes(file_bytes)
            .file_name(filename.to_string())
            .mime_str(mime_type)
            .map_err(|err| HttpError::InvalidRequest(format!("invalid MIME type: {err}")))?;

        let mut form = multipart::Form::new().part(file_field.to_string(), file_part);
        for (key, value) in fields {
            let value = match value {
                serde_json::Value::String(value) => value,
                value => value.to_string(),
            };
            form = form.text(key, value);
        }

        self.send_text(self.client(&url)?.post(url.clone()).multipart(form), &url)
    }

    fn client(&self, url: &reqwest::Url) -> Result<Client, HttpError> {
        self.require_credentials(url)?;
        super::client::blocking_for(url.as_str(), self.timeout).map_err(HttpError::InvalidRequest)
    }

    fn prepare_url(&self, url: &str) -> Result<reqwest::Url, HttpError> {
        let original = reqwest::Url::parse(url)
            .map_err(|err| HttpError::InvalidRequest(format!("invalid URL: {err}")))?;
        if original.scheme() != "https" {
            return Err(HttpError::InvalidRequest(
                "only https URLs are allowed".to_string(),
            ));
        }
        self.ensure_allowed_host(&original)?;
        let parsed = reqwest::Url::parse(&self.inject_string_credentials(url, &original)?)
            .map_err(|err| HttpError::InvalidRequest(format!("invalid URL: {err}")))?;
        if parsed.host_str() != original.host_str() {
            return Err(HttpError::InvalidRequest(
                "credential substitution changed the destination host".to_string(),
            ));
        }
        Ok(parsed)
    }

    fn authorize(
        &self,
        request: RequestBuilder,
        url: &reqwest::Url,
    ) -> Result<RequestBuilder, HttpError> {
        self.authorize_for(request, url, None)
    }

    fn authorize_for(
        &self,
        mut request: RequestBuilder,
        url: &reqwest::Url,
        account: Option<&str>,
    ) -> Result<RequestBuilder, HttpError> {
        let credentials = self.grants_for(url, account).collect::<Vec<_>>();
        if account.is_some() && credentials.is_empty() {
            return Err(HttpError::InvalidRequest(format!(
                "account is not allowed for host {}",
                url.host_str().unwrap_or_default()
            )));
        }
        for credential in credentials {
            if credential.placeholder.is_some() {
                continue;
            }
            if let Some(oauth) = &credential.oauth {
                request = request.bearer_auth(self.oauth_token(oauth)?);
                continue;
            }
            let Some(secret) = &credential.secret else {
                continue;
            };
            let value = self
                .secrets
                .get_secret(secret)
                .ok_or_else(|| HttpError::MissingCredential(secret.clone()))?;
            request = request.header(
                credential.header.as_str(),
                format!("{}{}", credential.prefix, value),
            );
        }
        Ok(request)
    }

    fn authorize_async(
        &self,
        mut request: reqwest::RequestBuilder,
        url: &reqwest::Url,
    ) -> Result<reqwest::RequestBuilder, HttpError> {
        for credential in self.grants_for(url, None) {
            if credential.placeholder.is_some() {
                continue;
            }
            if let Some(oauth) = &credential.oauth {
                request = request.bearer_auth(self.oauth_token(oauth)?);
                continue;
            }
            let Some(secret) = &credential.secret else {
                continue;
            };
            let value = self
                .secrets
                .get_secret(secret)
                .ok_or_else(|| HttpError::MissingCredential(secret.clone()))?;
            request = request.header(
                credential.header.as_str(),
                format!("{}{}", credential.prefix, value),
            );
        }
        Ok(request)
    }

    async fn async_client(&self, url: &reqwest::Url) -> Result<reqwest::Client, HttpError> {
        self.require_credentials(url)?;
        super::client::async_for(url.as_str(), self.timeout)
            .await
            .map_err(HttpError::InvalidRequest)
    }

    fn require_credentials(&self, url: &reqwest::Url) -> Result<(), HttpError> {
        for grant in self.grants_for(url, None) {
            if let Some(oauth) = &grant.oauth
                && self
                    .secrets
                    .get_secret(&oauth.refresh_token_handle)
                    .is_none()
            {
                return Err(HttpError::MissingCredential(
                    oauth.refresh_token_handle.clone(),
                ));
            }
            if let Some(secret) = &grant.secret
                && self.secrets.get_secret(secret).is_none()
            {
                return Err(HttpError::MissingCredential(secret.clone()));
            }
        }
        Ok(())
    }

    fn oauth_token(&self, config: &OAuthGrantConfig) -> Result<String, HttpError> {
        let cache_key = format!(
            "{:?}\0{:?}\0{:?}\0{}",
            config.broker_provider, config.token_url, config.client_id, config.refresh_token_handle
        );
        if let Some(token) = self
            .oauth_tokens
            .lock()
            .expect("OAuth token cache")
            .get(&cache_key)
        {
            return Ok(token.clone());
        }
        let refresh_token = self
            .secrets
            .get_secret(&config.refresh_token_handle)
            .ok_or_else(|| HttpError::MissingCredential(config.refresh_token_handle.clone()))?;
        let client_secret = if config.broker_provider.is_some() {
            None
        } else {
            config
                .client_secret
                .clone()
                .map(Ok)
                .or_else(|| {
                    config.client_secret_handle.as_ref().map(|name| {
                        self.secrets
                            .get_secret(name)
                            .ok_or_else(|| HttpError::MissingCredential(name.clone()))
                    })
                })
                .transpose()?
        };
        let token =
            super::oauth::refresh_access_token(config, &refresh_token, client_secret.as_deref())
                .map_err(HttpError::Http)?;
        self.oauth_tokens
            .lock()
            .expect("OAuth token cache")
            .insert(cache_key, token.clone());
        Ok(token)
    }

    fn send_text(&self, request: RequestBuilder, url: &reqwest::Url) -> Result<String, HttpError> {
        let response = self
            .authorize(request, url)?
            .send()
            .map_err(sanitize_blocking_reqwest_error)?;
        let status = response.status();
        let body = response.text().map_err(sanitize_blocking_reqwest_error)?;

        if !status.is_success() {
            return Err(HttpError::Http(format!(
                "request failed with {status}: {body}"
            )));
        }

        Ok(body)
    }

    fn inject_string_credentials(
        &self,
        value: &str,
        url: &reqwest::Url,
    ) -> Result<String, HttpError> {
        let mut value = value.to_string();
        for credential in self.grants_for(url, None) {
            let Some(placeholder) = &credential.placeholder else {
                continue;
            };
            if !value.contains(placeholder) {
                continue;
            }
            let secret = credential.secret.as_ref().ok_or_else(|| {
                HttpError::InvalidRequest(format!(
                    "placeholder {placeholder} has no configured secret"
                ))
            })?;
            let secret_value = self
                .secrets
                .get_secret(secret)
                .ok_or_else(|| HttpError::MissingCredential(secret.clone()))?;
            value = value.replace(placeholder, &secret_value);
        }
        Ok(value)
    }

    fn grants_for<'a>(
        &'a self,
        url: &reqwest::Url,
        account: Option<&'a str>,
    ) -> impl Iterator<Item = &'a NetworkGrant> {
        let host = url.host_str().unwrap_or_default();
        self.grants.iter().filter(move |credential| {
            credential.host == host && credential.account.as_deref() == account
        })
    }

    fn ensure_allowed_host(&self, url: &reqwest::Url) -> Result<(), HttpError> {
        let host = url.host_str().unwrap_or_default();
        if self.grants.iter().any(|credential| credential.host == host) {
            return Ok(());
        }

        Err(HttpError::InvalidRequest(format!(
            "host is not allowed: {host}"
        )))
    }
}

pub type HttpTool<S> = NetworkGate<S>;
pub type CredentialInjection = NetworkGrant;
pub type CredentialInjectionConfig = NetworkGrantConfig;

fn sanitize_blocking_reqwest_error(err: reqwest::Error) -> HttpError {
    HttpError::Http(err.without_url().to_string())
}

fn sanitize_reqwest_error(err: reqwest::Error) -> HttpError {
    HttpError::Http(err.without_url().to_string())
}
