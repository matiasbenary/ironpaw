use std::{
    io::Write as _,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use axum::{extract::ConnectInfo, http::Request, middleware::Next, response::Response};

pub(super) async fn log_request(request: Request<axum::body::Body>, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(address)| address.to_string());
    let forwarded_for = request
        .headers()
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let log_path = request
        .extensions()
        .get::<AuthLogPath>()
        .map(|path| path.0.clone());

    let response = next.run(request).await;
    if method != axum::http::Method::OPTIONS
        && let Some(log_path) = log_path
    {
        append(
            &log_path,
            &entry(
                method.as_str(),
                &path,
                response.status().as_u16(),
                peer.as_deref(),
                forwarded_for.as_deref(),
            ),
        );
    }
    response
}

#[derive(Clone)]
pub(super) struct AuthLogPath(pub(super) PathBuf);

fn entry(
    method: &str,
    path: &str,
    status: u16,
    peer: Option<&str>,
    forwarded_for: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "timestamp": chrono::Utc::now().to_rfc3339(),
        "event": "http_authentication",
        "method": method,
        "path": path,
        "status": status,
        "outcome": outcome(status),
        "peer": peer,
        "forwarded_for": forwarded_for,
    })
}

fn outcome(status: u16) -> &'static str {
    match status {
        401 | 403 => "rejected",
        404 => "route_not_found",
        503 => "unavailable",
        _ => "accepted",
    }
}

fn append(path: &Path, entry: &serde_json::Value) {
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let _ = writeln!(file, "{entry}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_authentication_results() {
        assert_eq!(outcome(200), "accepted");
        assert_eq!(outcome(400), "accepted");
        assert_eq!(outcome(401), "rejected");
        assert_eq!(outcome(404), "route_not_found");
        assert_eq!(outcome(503), "unavailable");
    }

    #[test]
    fn entry_excludes_query_strings_and_credentials() {
        let value = entry("GET", "/client/status", 401, Some("127.0.0.1:1"), None);
        let encoded = value.to_string();
        assert_eq!(value["path"], "/client/status");
        assert!(!encoded.contains("token"));
        assert_eq!(value["outcome"], "rejected");
    }
}
