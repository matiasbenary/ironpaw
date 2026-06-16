use std::{net::IpAddr, time::Duration};

use async_trait::async_trait;

use crate::{
    message::ToolDefinition,
    tools::{self, InternalTool, ToolContext, ToolOutput},
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
            .build()
            .expect("http client should build");
        Self { client }
    }

    /// Returns the response body as text. HTTP error statuses become a tool
    /// error so the model gets the standard `{ok:false,error}` envelope.
    pub async fn execute(&self, params: WebFetchParams) -> Result<String, String> {
        let url = reqwest::Url::parse(&params.url).map_err(|err| format!("invalid url: {err}"))?;
        guard_url(&url)?;

        let response = self
            .client
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

/// Cheap non-security guards: HTTPS-only (http for loopback) and block
/// loopback/private/link-local IPs (basic SSRF defense).
fn guard_url(url: &reqwest::Url) -> Result<(), String> {
    let host = url.host_str().unwrap_or_default();
    let is_loopback_host = matches!(host, "localhost" | "127.0.0.1" | "::1");
    if url.scheme() != "https" && !(url.scheme() == "http" && is_loopback_host) {
        return Err("only https urls are allowed (http only for localhost)".to_string());
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !is_loopback_host && is_private_ip(&ip) {
            return Err("requests to private/internal addresses are not allowed".to_string());
        }
    }
    Ok(())
}

fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified()
        }
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified(),
    }
}

#[async_trait]
impl InternalTool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn tool_definition(&self) -> ToolDefinition {
        tools::tool_definition::<WebFetchParams>(
            self.name(),
            "Fetch a public URL (HTTP GET). Returns the response body as a \
             string (call JSON.parse on it if the body is JSON). Read-only.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput, String> {
        tools::execute_typed(args, |params: WebFetchParams| async {
            WebFetchTool::execute(self, params).await
        })
        .await
        .map(ToolOutput::json)
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
    async fn rejects_invalid_urls() {
        let result = WebFetchTool::new().execute(params("not a url")).await;
        assert!(result.is_err());
    }
}
