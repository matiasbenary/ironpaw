use std::{io, path::PathBuf};

use axum::{
    Router,
    extract::{DefaultBodyLimit, Request},
    http::{HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use tokio::net::TcpListener;

mod audit_log;
pub(crate) mod auth;

pub const DEFAULT_ADDR: &str = "127.0.0.1:7878";

pub(crate) async fn bind(addr: &str) -> io::Result<TcpListener> {
    TcpListener::bind(addr).await.map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to bind network server at {addr}: {error}"),
        )
    })
}

pub(crate) async fn preflight() -> StatusCode {
    StatusCode::NO_CONTENT
}

pub(crate) async fn cors(request: Request, next: Next) -> Response {
    let is_preflight = request.method() == Method::OPTIONS;
    let mut response = if is_preflight {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(request).await
    };
    let headers = response.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("authorization, content-type"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    response
}

pub(crate) async fn run(
    listener: TcpListener,
    addr: &str,
    app: Router,
    audit_log_path: PathBuf,
    connection_token: String,
) -> io::Result<()> {
    let app = app
        .merge(super::oauth::routes(connection_token))
        .layer(DefaultBodyLimit::max(56 * 1024 * 1024))
        .layer(axum::middleware::from_fn(audit_log::log_request))
        .layer(axum::Extension(audit_log::AuthLogPath(audit_log_path)));

    println!("network server listening on http://{addr}");

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .map_err(io::Error::other)
}
