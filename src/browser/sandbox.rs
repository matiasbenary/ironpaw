use std::{path::Path, process::Stdio, time::Duration};

use chromiumoxide::{Browser, Handler};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
};

use crate::network::proxy::{ConnectProxy, TcpToUnixRelay};

pub(super) struct BrowserSandbox {
    process: Child,
    outbound: Option<ConnectProxy>,
    devtools: Option<TcpToUnixRelay>,
    runtime: std::path::PathBuf,
}

impl Drop for BrowserSandbox {
    fn drop(&mut self) {
        let _ = self.process.start_kill();
        self.devtools.take();
        self.outbound.take();
        let _ = std::fs::remove_file(self.runtime.join("devtools.sock"));
        let _ = std::fs::remove_dir(&self.runtime);
    }
}

pub(super) fn ensure_supported() -> Result<(), String> {
    if cfg!(target_os = "linux") {
        Ok(())
    } else {
        Err("browser isolation is only available on Linux".to_string())
    }
}

pub(super) async fn launch(
    chrome: &Path,
    profile: &Path,
) -> Result<(Browser, Handler, BrowserSandbox), String> {
    ensure_supported()?;
    let runtime = std::env::temp_dir().join(format!("ironpaw-browser-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&runtime).map_err(|error| error.to_string())?;
    let outbound_socket = runtime.join("outbound.sock");
    let devtools_socket = runtime.join("devtools.sock");
    let outbound = ConnectProxy::public_unix(outbound_socket).await?;
    let devtools = TcpToUnixRelay::start(devtools_socket).await?;
    let mut process = command(chrome, profile, &runtime)?
        .spawn()
        .map_err(|error| format!("failed to start isolated browser: {error}"))?;
    let websocket = wait_for_devtools(devtools.address(), &mut process).await?;
    let (browser, handler) = Browser::connect(websocket)
        .await
        .map_err(|error| format!("failed to connect to isolated browser: {error}"))?;
    Ok((
        browser,
        handler,
        BrowserSandbox {
            process,
            outbound: Some(outbound),
            devtools: Some(devtools),
            runtime,
        },
    ))
}

fn command(chrome: &Path, profile: &Path, runtime: &Path) -> Result<Command, String> {
    if !cfg!(target_os = "linux") {
        return Err("browser isolation is only available on Linux".to_string());
    }
    let bubblewrap = ["/usr/bin/bwrap", "/bin/bwrap"]
        .into_iter()
        .map(Path::new)
        .find(|path| path.is_file())
        .ok_or_else(|| "bubblewrap is required for browser isolation".to_string())?;
    let chrome = chrome
        .canonicalize()
        .map_err(|error| format!("failed to resolve browser executable: {error}"))?;
    let chrome_dir = chrome
        .parent()
        .ok_or_else(|| "browser executable has no parent directory".to_string())?;
    let chrome_name = chrome
        .file_name()
        .ok_or_else(|| "browser executable has no filename".to_string())?;
    let profile_metadata = std::fs::symlink_metadata(profile)
        .map_err(|error| format!("failed to inspect browser profile: {error}"))?;
    if profile_metadata.file_type().is_symlink() || !profile_metadata.is_dir() {
        return Err("browser profile must be a directory".to_string());
    }
    let profile = profile
        .canonicalize()
        .map_err(|error| format!("failed to resolve browser profile: {error}"))?;
    let runtime = runtime
        .canonicalize()
        .map_err(|error| format!("failed to resolve browser runtime: {error}"))?;
    let bridge = std::env::current_exe()
        .map_err(|error| format!("failed to resolve Ironpaw executable: {error}"))?
        .canonicalize()
        .map_err(|error| format!("failed to resolve Ironpaw executable: {error}"))?;

    let mut command = Command::new(bubblewrap);
    command.args([
        "--die-with-parent",
        "--new-session",
        "--unshare-all",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--tmpfs",
        "/tmp",
        "--tmpfs",
        "/dev/shm",
        "--dir",
        "/browser",
        "--ro-bind",
    ]);
    command.arg(chrome_dir).arg("/browser");
    command.args(["--dir", "/profile", "--bind"]);
    command.arg(profile).arg("/profile");
    command.args(["--dir", "/ironpaw", "--dir", "/ironpaw/run", "--bind"]);
    command.arg(runtime).arg("/ironpaw/run");
    command.args(["--dir", "/ironpaw/bin", "--ro-bind"]);
    command.arg(bridge).arg("/ironpaw/bin/bridge");
    for path in ["/usr", "/bin", "/lib", "/lib64"] {
        if Path::new(path).exists() {
            command.arg("--ro-bind").arg(path).arg(path);
        }
    }
    command.args(["--dir", "/etc"]);
    for path in ["/etc/ssl", "/etc/pki", "/etc/fonts"] {
        if Path::new(path).exists() {
            command.arg("--ro-bind").arg(path).arg(path);
        }
    }
    command.args([
        "--setenv",
        "HOME",
        "/profile",
        "--",
        "/ironpaw/bin/bridge",
        crate::network::proxy_bridge::BROWSER_ARGUMENT,
        "/ironpaw/run/outbound.sock",
        "/ironpaw/run/devtools.sock",
    ]);
    command.arg(Path::new("/browser").join(chrome_name));
    command.args([
        "--headless",
        "--hide-scrollbars",
        "--mute-audio",
        "--no-sandbox",
        "--disable-setuid-sandbox",
        "--remote-debugging-address=127.0.0.1",
        "--remote-debugging-port=17832",
        "--user-data-dir=/profile",
        "--proxy-server=http://127.0.0.1:17831",
        "--proxy-bypass-list=<-loopback>",
        "--disable-quic",
        "--dns-prefetch-disable",
        "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
        "--enable-logging",
        "--log-file=/profile/chrome.log",
        "about:blank",
    ]);
    command
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    Ok(command)
}

async fn wait_for_devtools(
    address: std::net::SocketAddr,
    process: &mut Child,
) -> Result<String, String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = process.try_wait().map_err(|error| error.to_string())? {
            return Err(format!("isolated browser exited before startup: {status}"));
        }
        if let Ok(url) = devtools_url(address).await {
            return Ok(url);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("isolated browser did not expose DevTools within 20 seconds".to_string());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn devtools_url(address: std::net::SocketAddr) -> Result<String, String> {
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .map_err(|error| error.to_string())?;
    stream
        .write_all(b"GET /json/version HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .map_err(|error| error.to_string())?;
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(1), stream.read_to_end(&mut response))
        .await
        .map_err(|_| "DevTools request timed out".to_string())?
        .map_err(|error| error.to_string())?;
    let body = response
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .map(|index| &response[index + 4..])
        .ok_or_else(|| "invalid DevTools response".to_string())?;
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|error| error.to_string())?;
    let url = value["webSocketDebuggerUrl"]
        .as_str()
        .ok_or_else(|| "DevTools response has no WebSocket URL".to_string())?;
    let mut url = reqwest::Url::parse(url).map_err(|error| error.to_string())?;
    url.set_host(Some("127.0.0.1"))
        .map_err(|_| "invalid DevTools host".to_string())?;
    url.set_port(Some(address.port()))
        .map_err(|_| "invalid DevTools port".to_string())?;
    Ok(url.to_string())
}
