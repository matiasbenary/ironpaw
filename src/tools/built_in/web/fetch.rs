use async_trait::async_trait;

use crate::{
    network::public::PublicHttp,
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
    http: PublicHttp,
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WebFetchTool {
    pub fn new() -> Self {
        Self {
            http: PublicHttp::new(),
        }
    }

    /// Returns the response body as text. HTTP error statuses become a tool
    /// error so the model gets the standard `{ok:false,error}` envelope.
    pub async fn execute(&self, params: WebFetchParams) -> Result<String, String> {
        let response = self.http.get(&params.url, MAX_BODY_BYTES, false).await?;
        let body = String::from_utf8_lossy(&response.body);
        if !(200..300).contains(&response.status) {
            return Err(format!(
                "request failed with status {}: {body}",
                response.status
            ));
        }
        Ok(body.into_owned())
    }
}

#[async_trait]
impl InternalTool for WebFetchTool {
    tools::tool_metadata!(
        "web_fetch",
        WebFetchParams,
        "Use only to fetch a known public URL (HTTP GET); never use it to \
             query a search engine. Returns the response body as a string (call \
             JSON.parse on it if the body is JSON). Read-only; it does not \
             execute JavaScript or interact with the page."
    );

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ExecutionContext,
        responder: ToolResponder,
    ) {
        responder.respond(
            tools::execute_typed(args, |params: WebFetchParams| async {
                WebFetchTool::execute(self, params).await
            })
            .await,
        );
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
}
