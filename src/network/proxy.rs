use std::{
    collections::HashSet,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream, UnixListener},
    task::{JoinHandle, JoinSet},
};

use super::policy::resolve_public;

const MAX_REQUEST_HEADER_BYTES: usize = 8 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) struct ConnectProxy {
    tcp_address: Option<SocketAddr>,
    socket_path: Option<PathBuf>,
    task: JoinHandle<()>,
}

impl ConnectProxy {
    pub(crate) async fn public_tcp() -> Result<Self, String> {
        Self::restricted_tcp(Vec::new(), false).await
    }

    #[cfg(debug_assertions)]
    pub(crate) async fn shell_tcp(allowed_hosts: Vec<String>) -> Result<Self, String> {
        Self::restricted_tcp(allowed_hosts, true).await
    }

    async fn restricted_tcp(
        allowed_hosts: Vec<String>,
        enforce_allowlist: bool,
    ) -> Result<Self, String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|error| format!("failed to start network proxy: {error}"))?;
        let address = listener
            .local_addr()
            .map_err(|error| format!("failed to read network proxy address: {error}"))?;
        let task = tokio::spawn(run_tcp(
            listener,
            HostPolicy::new(allowed_hosts, enforce_allowlist),
        ));
        Ok(Self {
            tcp_address: Some(address),
            socket_path: None,
            task,
        })
    }

    pub(crate) async fn shell_unix(
        socket_path: PathBuf,
        allowed_hosts: Vec<String>,
    ) -> Result<Self, String> {
        let listener = UnixListener::bind(&socket_path)
            .map_err(|error| format!("failed to start network proxy: {error}"))?;
        let task = tokio::spawn(run_unix(listener, HostPolicy::new(allowed_hosts, true)));
        Ok(Self {
            tcp_address: None,
            socket_path: Some(socket_path),
            task,
        })
    }

    pub(crate) fn tcp_address(&self) -> SocketAddr {
        self.tcp_address.expect("TCP proxy has an address")
    }

    pub(crate) fn socket_path(&self) -> &Path {
        self.socket_path.as_deref().expect("Unix proxy has a path")
    }
}

impl Drop for ConnectProxy {
    fn drop(&mut self) {
        self.task.abort();
        if let Some(path) = &self.socket_path {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[derive(Clone)]
struct HostPolicy {
    allowed_hosts: Arc<HashSet<String>>,
    enforce_allowlist: bool,
}

impl HostPolicy {
    fn new(allowed_hosts: Vec<String>, enforce_allowlist: bool) -> Self {
        Self {
            allowed_hosts: Arc::new(allowed_hosts.into_iter().collect()),
            enforce_allowlist,
        }
    }

    fn allows(&self, host: &str) -> bool {
        !self.enforce_allowlist || self.allowed_hosts.contains(&host.to_ascii_lowercase())
    }
}

async fn run_tcp(listener: TcpListener, policy: HostPolicy) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((client, _)) = accepted else {
                    break;
                };
                let policy = policy.clone();
                connections.spawn(async move { handle_connection(client, &policy).await });
            }
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}

async fn run_unix(listener: UnixListener, policy: HostPolicy) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((client, _)) = accepted else {
                    break;
                };
                let policy = policy.clone();
                connections.spawn(async move { handle_connection(client, &policy).await });
            }
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}

async fn handle_connection<S>(mut client: S, policy: &HostPolicy)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if let Err(error) = proxy_connection(&mut client, policy).await {
        let response = match error {
            ProxyError::BadRequest => {
                "HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
            }
            ProxyError::Forbidden => {
                "HTTP/1.1 403 Forbidden\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
            }
            ProxyError::Unavailable => {
                "HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
            }
        };
        let _ = client.write_all(response.as_bytes()).await;
        let _ = client.shutdown().await;
    }
}

async fn proxy_connection<S>(client: &mut S, policy: &HostPolicy) -> Result<(), ProxyError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let request = tokio::time::timeout(REQUEST_TIMEOUT, read_request(client))
        .await
        .map_err(|_| ProxyError::BadRequest)??;
    let header_end = find_header_end(&request).ok_or(ProxyError::BadRequest)?;
    let first_line_end = request
        .windows(2)
        .position(|bytes| bytes == b"\r\n")
        .ok_or(ProxyError::BadRequest)?;
    let first_line =
        std::str::from_utf8(&request[..first_line_end]).map_err(|_| ProxyError::BadRequest)?;
    let mut parts = first_line.split_whitespace();
    let method = parts.next().ok_or(ProxyError::BadRequest)?;
    let target = parts.next().ok_or(ProxyError::BadRequest)?;
    let version = parts.next().ok_or(ProxyError::BadRequest)?;
    if method != "CONNECT" || !version.starts_with("HTTP/1.") || parts.next().is_some() {
        return Err(ProxyError::BadRequest);
    }
    let (host, port) = parse_target(target).ok_or(ProxyError::BadRequest)?;
    if !policy.allows(host) {
        return Err(ProxyError::Forbidden);
    }
    let addresses = resolve_public(host, port)
        .await
        .map_err(|_| ProxyError::Forbidden)?;
    let mut upstream = connect(&addresses).await.ok_or(ProxyError::Unavailable)?;

    let buffered = &request[header_end..];
    if !buffered.is_empty() {
        upstream
            .write_all(buffered)
            .await
            .map_err(|_| ProxyError::Unavailable)?;
    }
    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await
        .map_err(|_| ProxyError::Unavailable)?;
    let _ = tokio::io::copy_bidirectional(client, &mut upstream).await;
    Ok(())
}

async fn read_request<S>(client: &mut S) -> Result<Vec<u8>, ProxyError>
where
    S: AsyncRead + Unpin,
{
    let mut request = Vec::new();
    let mut chunk = [0_u8; 1024];
    while request.len() < MAX_REQUEST_HEADER_BYTES {
        let read = client
            .read(&mut chunk)
            .await
            .map_err(|_| ProxyError::BadRequest)?;
        if read == 0 {
            return Err(ProxyError::BadRequest);
        }
        request.extend_from_slice(&chunk[..read]);
        if find_header_end(&request).is_some() {
            return Ok(request);
        }
    }
    Err(ProxyError::BadRequest)
}

fn find_header_end(request: &[u8]) -> Option<usize> {
    request
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .map(|position| position + 4)
}

fn parse_target(target: &str) -> Option<(&str, u16)> {
    let (host, port) = if let Some(rest) = target.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = &rest[..end];
        let port = rest[end + 1..].strip_prefix(':')?;
        (host, port)
    } else {
        target.rsplit_once(':')?
    };
    if host.is_empty() || host.contains(['/', '@']) {
        return None;
    }
    let port = port.parse().ok()?;
    (port != 0).then_some((host, port))
}

async fn connect(addresses: &[SocketAddr]) -> Option<TcpStream> {
    for address in addresses {
        if let Ok(Ok(stream)) =
            tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(address)).await
        {
            return Some(stream);
        }
    }
    None
}

#[derive(Clone, Copy)]
enum ProxyError {
    BadRequest,
    Forbidden,
    Unavailable,
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::{ConnectProxy, parse_target};

    #[test]
    fn parses_connect_targets() {
        assert_eq!(parse_target("example.com:443"), Some(("example.com", 443)));
        assert_eq!(
            parse_target("[2606:4700::1111]:8443"),
            Some(("2606:4700::1111", 8443))
        );
        assert_eq!(parse_target("https://example.com"), None);
        assert_eq!(parse_target("user@example.com:443"), None);
        assert_eq!(parse_target("example.com:0"), None);
    }

    #[tokio::test]
    async fn rejects_hosts_outside_shell_allowlist() {
        let proxy = ConnectProxy::shell_tcp(vec!["api.github.com".to_string()])
            .await
            .unwrap();
        let mut client = tokio::net::TcpStream::connect(proxy.tcp_address())
            .await
            .unwrap();
        client
            .write_all(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert!(response.starts_with(b"HTTP/1.1 403 Forbidden"));
    }

    #[tokio::test]
    async fn rejects_internal_connect_targets() {
        let proxy = ConnectProxy::public_tcp().await.unwrap();
        let mut client = tokio::net::TcpStream::connect(proxy.tcp_address())
            .await
            .unwrap();
        client
            .write_all(b"CONNECT 127.0.0.1:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert!(response.starts_with(b"HTTP/1.1 403 Forbidden"));
    }
}
