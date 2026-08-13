use std::time::Duration;

use reqwest::{Url, redirect::Policy};

use super::policy::{resolve_public, resolve_public_blocking};

fn destination(url: &str) -> Result<(Url, String, u16), String> {
    let url = Url::parse(url).map_err(|err| format!("invalid URL: {err}"))?;
    if url.scheme() != "https" {
        return Err("only https URLs are allowed".to_string());
    }
    let host = url
        .host_str()
        .ok_or_else(|| "destination has no host".to_string())?
        .to_string();
    let port = url.port_or_known_default().unwrap_or(443);
    Ok((url, host, port))
}

pub(crate) async fn async_for(url: &str, timeout: Duration) -> Result<reqwest::Client, String> {
    let (_, host, port) = destination(url)?;
    let addresses = resolve_public(&host, port).await?;
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
    blocking_for_headers(url, timeout, reqwest::header::HeaderMap::new())
}

pub(crate) fn blocking_for_headers(
    url: &str,
    timeout: Duration,
    headers: reqwest::header::HeaderMap,
) -> Result<reqwest::blocking::Client, String> {
    let (_, host, port) = destination(url)?;
    let addresses = resolve_public_blocking(&host, port)?;
    reqwest::blocking::Client::builder()
        .timeout(timeout)
        .redirect(Policy::none())
        .resolve_to_addrs(&host, &addresses)
        .default_headers(headers)
        .build()
        .map_err(|err| err.without_url().to_string())
}
