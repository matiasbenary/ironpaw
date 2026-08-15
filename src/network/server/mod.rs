use std::{io, path::PathBuf};

use axum::{Router, extract::DefaultBodyLimit};
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

pub(crate) async fn run(
    listener: TcpListener,
    addr: &str,
    app: Router,
    audit_log_path: PathBuf,
) -> io::Result<()> {
    let app = app
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
