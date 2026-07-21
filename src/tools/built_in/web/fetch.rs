use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use async_trait::async_trait;

use crate::{
    message::ToolDefinition,
    tools::{self, ExecutionContext, InternalTool, ToolResponder},
};

/// Responses are truncated to keep tool results model-sized.
const MAX_BODY_BYTES: usize = 100_000;

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct WebFetchParams {
    /// URL to fetch. HTTPS only (http allowed for localhost).
    pub url: String,
}

/// Read-only web retrieval for the agent: GET any public URL. It carries no
/// credentials, so a request to an attacker's host leaks nothing. Writes
/// (POST/PUT/...) and authenticated calls are intentionally not here — those
/// will go through the shell and dedicated tools.
pub struct WebFetchTool {
    client: reqwest::Client,
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WebFetchTool {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("http client should build");
        Self { client }
    }

    /// Returns the response body as text. HTTP error statuses become a tool
    /// error so the model gets the standard `{ok:false,error}` envelope.
    pub async fn execute(&self, params: WebFetchParams) -> Result<String, String> {
        let url = reqwest::Url::parse(&params.url).map_err(|err| format!("invalid url: {err}"))?;
        let resolved = guard_url(&url).await?;
        let client = match resolved {
            Some(addrs) => pinned_client(url.host_str().unwrap_or_default(), &addrs)?,
            None => self.client.clone(),
        };

        let response = client
            .get(url)
            .send()
            .await
            .map_err(|err| format!("request failed: {}", err.without_url()))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|err| format!("failed to read body: {}", err.without_url()))?;
        let body = truncate_on_char_boundary(body, MAX_BODY_BYTES);

        if !status.is_success() {
            return Err(format!(
                "request failed with status {}: {body}",
                status.as_u16()
            ));
        }
        Ok(body)
    }
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

/// HTTPS-only (http for loopback) and block loopback/private/link-local
/// resolved addresses. Redirects are disabled at the client, so a public URL
/// cannot bounce the request to an internal target after this check.
pub(crate) async fn guard_url(url: &reqwest::Url) -> Result<Option<Vec<SocketAddr>>, String> {
    let host = url.host_str().unwrap_or_default();
    let is_loopback_host = matches!(host, "localhost" | "127.0.0.1" | "::1");
    if url.scheme() != "https" && !(url.scheme() == "http" && is_loopback_host) {
        return Err("only https urls are allowed (http only for localhost)".to_string());
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_private_ip(&ip) && !(url.scheme() == "http" && is_loopback_host) {
            return Err("requests to private/internal addresses are not allowed".to_string());
        }
        return Ok(None);
    }
    if is_loopback_host {
        if url.scheme() == "http" {
            return Ok(None);
        }
        return Err("requests to private/internal addresses are not allowed".to_string());
    }
    let port = url.port_or_known_default().unwrap_or(443);
    let addresses: Vec<_> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|err| format!("failed to resolve host: {err}"))?
        .collect();
    if addresses.is_empty() {
        return Err("host resolved to no addresses".to_string());
    }
    for address in &addresses {
        if is_private_ip(&address.ip()) {
            return Err("requests to private/internal addresses are not allowed".to_string());
        }
    }
    Ok(Some(addresses))
}

pub(crate) fn pinned_client(host: &str, addrs: &[SocketAddr]) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .resolve_to_addrs(host, addrs)
        .build()
        .map_err(|err| format!("http client should build: {err}"))
}

fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || is_unique_local_v6(v6)
                || is_unicast_link_local_v6(v6)
        }
    }
}

fn is_unique_local_v6(ip: &std::net::Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xfe00) == 0xfc00
}

fn is_unicast_link_local_v6(ip: &std::net::Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xffc0) == 0xfe80
}

#[async_trait]
impl InternalTool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn tool_definition(&self) -> ToolDefinition {
        tools::tool_definition::<WebFetchParams>(
            self.name(),
            "Use only to fetch a known public URL (HTTP GET); never use it to \
             query a search engine. Returns the response body as a string (call \
             JSON.parse on it if the body is JSON). Read-only; it does not \
             execute JavaScript or interact with the page.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ExecutionContext,
        responder: ToolResponder,
    ) {
        responder.respond(tools::execute_typed(args, |params: WebFetchParams| async {
            WebFetchTool::execute(self, params).await
        }).await);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(url: &str) -> WebFetchParams {
        WebFetchParams {
            url: url.to_string(),
        }
    }

    #[tokio::test]
    async fn rejects_plain_http_for_non_loopback() {
        let result = WebFetchTool::new()
            .execute(params("http://example.com"))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn rejects_private_addresses() {
        let result = WebFetchTool::new()
            .execute(params("https://169.254.169.254/latest/meta-data"))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn rejects_localhost_dns_name_for_https() {
        let result = WebFetchTool::new()
            .execute(params("https://localhost/"))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn does_not_follow_redirects() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0; 1024];
            let _ = socket.read(&mut buf).await;
            socket
                .write_all(
                    b"HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1/private\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });

        let result = WebFetchTool::new().execute(params(&url)).await;

        assert!(result.unwrap_err().contains("302"));
    }

    #[tokio::test]
    async fn rejects_invalid_urls() {
        let result = WebFetchTool::new().execute(params("not a url")).await;
        assert!(result.is_err());
    }
}
