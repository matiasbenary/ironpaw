use std::{net::SocketAddr, time::Duration};

use reqwest::{Client, Url, redirect::Policy};

use super::policy::{is_public_ip, resolve_public};

pub(crate) struct PublicResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub content_type: String,
}

#[derive(Clone)]
pub(crate) struct PublicHttp {
    client: Client,
}

impl PublicHttp {
    pub fn new() -> Self {
        Self {
            client: client(None).expect("public HTTP client"),
        }
    }

    pub async fn get(
        &self,
        url: &str,
        limit: usize,
        reject_oversize: bool,
    ) -> Result<PublicResponse, String> {
        let url = Url::parse(url).map_err(|err| format!("invalid url: {err}"))?;
        let resolved = guard_url(&url).await?;
        let client = match resolved {
            Some(addresses) => client(Some((url.host_str().unwrap_or_default(), &addresses)))?,
            None => self.client.clone(),
        };
        let mut response = client
            .get(url.clone())
            .send()
            .await
            .map_err(|err| format!("request failed: {}", err.without_url()))?;
        let status = response.status().as_u16();
        if reject_oversize
            && response
                .content_length()
                .is_some_and(|size| size > limit as u64)
        {
            return Err(format!("response is too large (> {} bytes)", limit));
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("application/octet-stream")
            .split(';')
            .next()
            .unwrap_or("application/octet-stream")
            .trim()
            .to_string();
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|err| format!("failed to read body: {}", err.without_url()))?
        {
            if body.len() + chunk.len() > limit {
                if reject_oversize {
                    return Err(format!("response is too large (> {} bytes)", limit));
                }
                body.extend_from_slice(&chunk[..limit - body.len()]);
                break;
            }
            body.extend_from_slice(&chunk);
        }
        Ok(PublicResponse {
            status,
            body,
            content_type,
        })
    }
}

pub(crate) async fn guard_url(url: &Url) -> Result<Option<Vec<SocketAddr>>, String> {
    let host = url.host_str().unwrap_or_default();
    let loopback = matches!(host, "localhost" | "127.0.0.1" | "::1");
    let local_http = cfg!(feature = "local-development") && url.scheme() == "http" && loopback;
    if url.scheme() != "https" && !local_http {
        return Err("only https urls are allowed".to_string());
    }
    if let Ok(ip) = host.parse() {
        if !(is_public_ip(ip) || local_http) {
            return Err("requests to private/internal addresses are not allowed".to_string());
        }
        return Ok(None);
    }
    if loopback {
        return local_http
            .then_some(None)
            .ok_or_else(|| "requests to private/internal addresses are not allowed".to_string());
    }
    resolve_public(host, url.port_or_known_default().unwrap_or(443))
        .await
        .map(Some)
}

fn client(resolve: Option<(&str, &[SocketAddr])>) -> Result<Client, String> {
    let mut builder = Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(Policy::none());
    if let Some((host, addresses)) = resolve {
        builder = builder.resolve_to_addrs(host, addresses);
    }
    builder
        .build()
        .map_err(|err| format!("http client should build: {err}"))
}
