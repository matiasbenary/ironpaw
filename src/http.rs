use std::{fmt, time::Duration};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use reqwest::{Client, Method, blocking::multipart};

pub trait SecretLookup: Clone + Send + Sync + 'static {
    fn get_secret(&self, name: &str) -> Option<String>;
}

#[derive(Clone, Debug)]
pub struct CredentialInjection {
    pub host: String,
    pub secret: String,
    pub header: String,
    pub prefix: String,
    pub placeholder: Option<String>,
}

impl CredentialInjection {
    pub fn bearer(host: impl Into<String>, secret: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            secret: secret.into(),
            header: "Authorization".to_string(),
            prefix: "Bearer ".to_string(),
            placeholder: None,
        }
    }

    pub fn url_placeholder(
        host: impl Into<String>,
        placeholder: impl Into<String>,
        secret: impl Into<String>,
    ) -> Self {
        Self {
            host: host.into(),
            secret: secret.into(),
            header: String::new(),
            prefix: String::new(),
            placeholder: Some(placeholder.into()),
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
pub struct HttpTool<S> {
    client: Client,
    secrets: S,
    credentials: Vec<CredentialInjection>,
}

impl<S> HttpTool<S>
where
    S: SecretLookup,
{
    pub fn new(secrets: S, credentials: Vec<CredentialInjection>) -> Result<Self, HttpError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(sanitize_reqwest_error)?;

        Ok(Self {
            client,
            secrets,
            credentials,
        })
    }

    pub async fn post_json<T>(&self, url: &str, body: &T) -> Result<String, HttpError>
    where
        T: serde::Serialize + ?Sized,
    {
        let url = self.inject_url_credentials(url)?;
        let parsed = reqwest::Url::parse(&url)
            .map_err(|err| HttpError::InvalidRequest(format!("invalid URL: {err}")))?;

        if parsed.scheme() != "https" && !is_loopback_http(&parsed) {
            return Err(HttpError::InvalidRequest(
                "only https URLs are allowed, except loopback http".to_string(),
            ));
        }
        self.ensure_allowed_host(&parsed)?;

        let mut request = self.client.post(parsed.clone()).json(body);

        for credential in self.credentials_for(&parsed) {
            if credential.placeholder.is_some() {
                continue;
            }
            let value = self
                .secrets
                .get_secret(&credential.secret)
                .ok_or_else(|| HttpError::MissingCredential(credential.secret.clone()))?;
            request = request.header(
                credential.header.as_str(),
                format!("{}{}", credential.prefix, value),
            );
        }

        let response = request.send().await.map_err(sanitize_reqwest_error)?;
        let status = response.status();
        let body = response.text().await.map_err(sanitize_reqwest_error)?;

        if !status.is_success() {
            return Err(HttpError::Http(format!(
                "request failed with {status}: {body}"
            )));
        }

        Ok(body)
    }

    pub fn post_json_blocking<T>(&self, url: &str, body: &T) -> Result<String, HttpError>
    where
        T: serde::Serialize + ?Sized,
    {
        let url = self.inject_url_credentials(url)?;
        let parsed = reqwest::Url::parse(&url)
            .map_err(|err| HttpError::InvalidRequest(format!("invalid URL: {err}")))?;

        if parsed.scheme() != "https" && !is_loopback_http(&parsed) {
            return Err(HttpError::InvalidRequest(
                "only https URLs are allowed, except loopback http".to_string(),
            ));
        }
        self.ensure_allowed_host(&parsed)?;

        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(sanitize_blocking_reqwest_error)?;
        let mut request = client.post(parsed.clone()).json(body);

        for credential in self.credentials_for(&parsed) {
            if credential.placeholder.is_some() {
                continue;
            }
            let value = self
                .secrets
                .get_secret(&credential.secret)
                .ok_or_else(|| HttpError::MissingCredential(credential.secret.clone()))?;
            request = request.header(
                credential.header.as_str(),
                format!("{}{}", credential.prefix, value),
            );
        }

        let response = request.send().map_err(sanitize_blocking_reqwest_error)?;
        let status = response.status();
        let body = response.text().map_err(sanitize_blocking_reqwest_error)?;

        if !status.is_success() {
            return Err(HttpError::Http(format!(
                "request failed with {status}: {body}"
            )));
        }

        Ok(body)
    }

    pub fn post_json_string_blocking(
        &self,
        url: &str,
        body_json: &str,
    ) -> Result<String, HttpError> {
        let body_json = self.inject_string_credentials(body_json)?;
        let body = serde_json::from_str::<serde_json::Value>(&body_json)
            .map_err(|err| HttpError::InvalidRequest(format!("invalid JSON body: {err}")))?;
        self.post_json_blocking(url, &body)
    }

    pub fn request_bytes_string_blocking(
        &self,
        method: &str,
        url: &str,
        body_json: &str,
    ) -> Result<Vec<u8>, HttpError> {
        let method = method
            .parse::<Method>()
            .map_err(|err| HttpError::InvalidRequest(format!("invalid HTTP method: {err}")))?;
        if method != Method::GET && method != Method::POST {
            return Err(HttpError::InvalidRequest(
                "only GET and POST are supported".to_string(),
            ));
        }

        let url = self.inject_url_credentials(url)?;
        let parsed = reqwest::Url::parse(&url)
            .map_err(|err| HttpError::InvalidRequest(format!("invalid URL: {err}")))?;

        if parsed.scheme() != "https" && !is_loopback_http(&parsed) {
            return Err(HttpError::InvalidRequest(
                "only https URLs are allowed, except loopback http".to_string(),
            ));
        }
        self.ensure_allowed_host(&parsed)?;

        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(sanitize_blocking_reqwest_error)?;
        let mut request = client.request(method.clone(), parsed.clone());
        if method == Method::POST {
            let body_json = self.inject_string_credentials(body_json)?;
            let body = serde_json::from_str::<serde_json::Value>(&body_json)
                .map_err(|err| HttpError::InvalidRequest(format!("invalid JSON body: {err}")))?;
            request = request.json(&body);
        }

        for credential in self.credentials_for(&parsed) {
            if credential.placeholder.is_some() {
                continue;
            }
            let value = self
                .secrets
                .get_secret(&credential.secret)
                .ok_or_else(|| HttpError::MissingCredential(credential.secret.clone()))?;
            request = request.header(
                credential.header.as_str(),
                format!("{}{}", credential.prefix, value),
            );
        }

        let response = request.send().map_err(sanitize_blocking_reqwest_error)?;
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
        let url = self.inject_url_credentials(url)?;
        let parsed = reqwest::Url::parse(&url)
            .map_err(|err| HttpError::InvalidRequest(format!("invalid URL: {err}")))?;

        if parsed.scheme() != "https" && !is_loopback_http(&parsed) {
            return Err(HttpError::InvalidRequest(
                "only https URLs are allowed, except loopback http".to_string(),
            ));
        }
        self.ensure_allowed_host(&parsed)?;

        let fields_json = self.inject_string_credentials(fields_json)?;
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

        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(sanitize_blocking_reqwest_error)?;
        let mut request = client.post(parsed.clone()).multipart(form);

        for credential in self.credentials_for(&parsed) {
            if credential.placeholder.is_some() {
                continue;
            }
            let value = self
                .secrets
                .get_secret(&credential.secret)
                .ok_or_else(|| HttpError::MissingCredential(credential.secret.clone()))?;
            request = request.header(
                credential.header.as_str(),
                format!("{}{}", credential.prefix, value),
            );
        }

        let response = request.send().map_err(sanitize_blocking_reqwest_error)?;
        let status = response.status();
        let body = response.text().map_err(sanitize_blocking_reqwest_error)?;

        if !status.is_success() {
            return Err(HttpError::Http(format!(
                "request failed with {status}: {body}"
            )));
        }

        Ok(body)
    }

    fn inject_url_credentials(&self, url: &str) -> Result<String, HttpError> {
        self.inject_string_credentials(url)
    }

    fn inject_string_credentials(&self, value: &str) -> Result<String, HttpError> {
        let mut value = value.to_string();
        for credential in &self.credentials {
            let Some(placeholder) = &credential.placeholder else {
                continue;
            };
            if !value.contains(placeholder) {
                continue;
            }
            let secret_value = self
                .secrets
                .get_secret(&credential.secret)
                .ok_or_else(|| HttpError::MissingCredential(credential.secret.clone()))?;
            value = value.replace(placeholder, &secret_value);
        }
        Ok(value)
    }

    fn credentials_for<'a>(
        &'a self,
        url: &reqwest::Url,
    ) -> impl Iterator<Item = &'a CredentialInjection> {
        let host = url.host_str().unwrap_or_default();
        self.credentials
            .iter()
            .filter(move |credential| credential.host == host)
    }

    fn ensure_allowed_host(&self, url: &reqwest::Url) -> Result<(), HttpError> {
        if is_loopback_http(url) {
            return Ok(());
        }

        let host = url.host_str().unwrap_or_default();
        if self
            .credentials
            .iter()
            .any(|credential| credential.host == host)
        {
            return Ok(());
        }

        Err(HttpError::InvalidRequest(format!(
            "host is not allowed: {host}"
        )))
    }
}

fn sanitize_reqwest_error(err: reqwest::Error) -> HttpError {
    HttpError::Http(err.without_url().to_string())
}

fn sanitize_blocking_reqwest_error(err: reqwest::Error) -> HttpError {
    HttpError::Http(err.without_url().to_string())
}

fn is_loopback_http(url: &reqwest::Url) -> bool {
    if url.scheme() != "http" {
        return false;
    }

    matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"))
}
