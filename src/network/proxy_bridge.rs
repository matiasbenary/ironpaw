use std::{ffi::OsString, process::Stdio};

use tokio::{
    io,
    net::{TcpListener, TcpStream, UnixStream},
    process::Command,
    task::JoinSet,
};

pub const ARGUMENT: &str = "__ironpaw_shell_proxy_bridge";
const ADDRESS: &str = "127.0.0.1:17831";

pub async fn run(args: Vec<OsString>) -> Result<i32, String> {
    if args.len() < 2 {
        return Err("shell proxy bridge requires a socket and command".to_string());
    }
    let socket = args[0].clone();
    let executable = args[1].clone();
    let listener = TcpListener::bind(ADDRESS)
        .await
        .map_err(|error| format!("failed to start shell proxy bridge: {error}"))?;
    let proxy_url = format!("http://{ADDRESS}");
    let mut command = Command::new(executable);
    command
        .args(&args[2..])
        .env("HTTP_PROXY", &proxy_url)
        .env("HTTPS_PROXY", &proxy_url)
        .env("ALL_PROXY", &proxy_url)
        .env("http_proxy", &proxy_url)
        .env("https_proxy", &proxy_url)
        .env("all_proxy", &proxy_url)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|error| format!("failed to start shell command: {error}"))?;
    let mut connections = JoinSet::new();

    loop {
        tokio::select! {
            status = child.wait() => {
                return status
                    .map_err(|error| format!("failed to wait for shell command: {error}"))
                    .map(|status| status.code().unwrap_or(1));
            }
            accepted = listener.accept() => {
                let (client, _) =
                    accepted.map_err(|error| format!("shell proxy bridge failed: {error}"))?;
                let socket = socket.clone();
                connections.spawn(async move {
                    let Ok(upstream) = UnixStream::connect(socket).await else {
                        return;
                    };
                    relay(client, upstream).await;
                });
            }
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}

async fn relay(mut client: TcpStream, mut upstream: UnixStream) {
    let _ = io::copy_bidirectional(&mut client, &mut upstream).await;
}
