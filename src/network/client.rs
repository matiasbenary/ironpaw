use std::{net::SocketAddr, time::Duration};

use reqwest::{Url, redirect::Policy};

use super::policy::{resolve_public, resolve_public_blocking};

fn destination(url: &str, allow_http_loopback: bool) -> Result<(Url, String, u16, bool), String> {
    let url = Url::parse(url).map_err(|err| format!("invalid URL: {err}"))?;
    let host = url
        .host_str()
        .ok_or_else(|| "destination has no host".to_string())?
        .to_string();
    let loopback = matches!(host.as_str(), "localhost" | "127.0.0.1");
    let local_http = cfg!(feature = "local-development")
        && allow_http_loopback
        && url.scheme() == "http"
        && loopback;
    if url.scheme() != "https" && !local_http {
        return Err("only https URLs are allowed".to_string());
    }
    let port = url.port_or_known_default().unwrap_or(443);
    Ok((url, host, port, loopback))
}

pub(crate) async fn async_for(url: &str, timeout: Duration) -> Result<reqwest::Client, String> {
    async_client(url, timeout, false).await
}

pub(crate) async fn async_for_https_or_loopback(
    url: &str,
    timeout: Duration,
) -> Result<reqwest::Client, String> {
    async_client(url, timeout, true).await
}

async fn async_client(
    url: &str,
    timeout: Duration,
    allow_loopback: bool,
) -> Result<reqwest::Client, String> {
    let (_, host, port, loopback) = destination(url, allow_loopback)?;
    let addresses = if loopback && allow_loopback {
        vec![SocketAddr::from(([127, 0, 0, 1], port))]
    } else {
        resolve_public(&host, port).await?
    };
    reqwest::Client::builder()
        .timeout(timeout)
        .redirect(Policy::none())
        .resolve_to_addrs(&host, &addresses)
        .build()
        .map_err(|err| err.without_url().to_string())
}

pub(crate) fn blocking_for(
    url: &str,
    timeout: Duration,
) -> Result<reqwest::blocking::Client, String> {
    blocking_client(url, timeout, reqwest::header::HeaderMap::new(), false)
}

pub(crate) fn blocking_for_https_or_loopback(
    url: &str,
    timeout: Duration,
) -> Result<reqwest::blocking::Client, String> {
    blocking_client(url, timeout, reqwest::header::HeaderMap::new(), true)
}

fn blocking_client(
    url: &str,
    timeout: Duration,
    headers: reqwest::header::HeaderMap,
    allow_loopback: bool,
) -> Result<reqwest::blocking::Client, String> {
    let (_, host, port, loopback) = destination(url, allow_loopback)?;
    let addresses = if loopback && allow_loopback {
        vec![SocketAddr::from(([127, 0, 0, 1], port))]
    } else {
        resolve_public_blocking(&host, port)?
    };
    reqwest::blocking::Client::builder()
        .timeout(timeout)
        .redirect(Policy::none())
        .resolve_to_addrs(&host, &addresses)
        .default_headers(headers)
        .build()
        .map_err(|err| err.without_url().to_string())
}

#[cfg(test)]
mod tests {
    use super::{async_for, async_for_https_or_loopback, destination};
    use std::time::Duration;

    #[tokio::test]
    async fn loopback_http_requires_explicit_permission() {
        assert!(destination("http://localhost:3000", false).is_err());
        assert_eq!(
            destination("http://localhost:3000", true).is_ok(),
            cfg!(feature = "local-development")
        );
        assert!(
            async_for("https://localhost:3000", Duration::from_secs(1))
                .await
                .is_err()
        );
        assert_eq!(
            async_for_https_or_loopback("http://localhost:3000", Duration::from_secs(1))
                .await
                .is_ok(),
            cfg!(feature = "local-development")
        );
        assert!(destination("http://localhost.evil:3000", true).is_err());
    }
}
