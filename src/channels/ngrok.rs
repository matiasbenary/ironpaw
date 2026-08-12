use std::{
    env, fmt, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use tokio::{
    io::{AsyncBufReadExt, BufReader, Lines},
    process::{Child, ChildStdout, Command as TokioCommand},
    task::JoinHandle,
    time::{Duration, Instant, timeout},
};

use crate::secrets::ChannelSecrets;

#[derive(Debug)]
pub enum NgrokError {
    InvalidConfig(String),
    Install(String),
    Start(String),
    UnsupportedPlatform(String),
}

impl fmt::Display for NgrokError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(message) => write!(f, "invalid ngrok config: {message}"),
            Self::Install(message) => write!(f, "failed to install ngrok: {message}"),
            Self::Start(message) => write!(f, "failed to start ngrok: {message}"),
            Self::UnsupportedPlatform(message) => {
                write!(f, "unsupported ngrok platform: {message}")
            }
        }
    }
}

impl std::error::Error for NgrokError {}

pub struct ActiveNgrok {
    pub public_url: String,
    _process: NgrokProcess,
}

pub async fn start_if_configured(
    runtime_root: &Path,
    secrets: &ChannelSecrets,
    local_addr: &str,
) -> Result<Option<ActiveNgrok>, NgrokError> {
    let Some(auth_token) = secrets.get("IRONPAW_NGROK_TOKEN") else {
        return Ok(None);
    };
    let domain = secrets.get("IRONPAW_NGROK_DOMAIN");
    start(runtime_root, local_addr, auth_token, domain)
        .await
        .map(Some)
}

async fn start(
    runtime_root: &Path,
    local_addr: &str,
    auth_token: String,
    domain: Option<String>,
) -> Result<ActiveNgrok, NgrokError> {
    let ngrok = ensure_ngrok(runtime_root)?;
    let mut command = TokioCommand::new(ngrok);
    command
        .args(["http", local_addr])
        .args(["--log", "stdout", "--log-format", "logfmt"])
        .env("NGROK_AUTHTOKEN", auth_token)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    if let Some(domain) = domain {
        command.args(["--domain", &domain]);
    }

    let mut child = command
        .spawn()
        .map_err(|err| NgrokError::Start(err.to_string()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| NgrokError::Start("failed to capture ngrok stdout".to_string()))?;
    let stderr = child.stderr.take();
    let mut stdout_lines = BufReader::new(stdout).lines();

    let public_url = read_public_url(&mut stdout_lines, Duration::from_secs(15))
        .await
        .inspect_err(|_| {
            child.start_kill().ok();
        })?;

    let stdout_drain =
        tokio::spawn(async move { while let Ok(Some(_line)) = stdout_lines.next_line().await {} });
    let stderr_drain = stderr.map(|stderr| {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(_line)) = lines.next_line().await {}
        })
    });

    Ok(ActiveNgrok {
        public_url,
        _process: NgrokProcess {
            child,
            _stdout_drain: stdout_drain,
            _stderr_drain: stderr_drain,
        },
    })
}

struct NgrokProcess {
    child: Child,
    _stdout_drain: JoinHandle<()>,
    _stderr_drain: Option<JoinHandle<()>>,
}

impl Drop for NgrokProcess {
    fn drop(&mut self) {
        self.child.start_kill().ok();
    }
}

async fn read_public_url(
    lines: &mut Lines<BufReader<ChildStdout>>,
    deadline_after: Duration,
) -> Result<String, NgrokError> {
    let deadline = Instant::now() + deadline_after;
    while Instant::now() < deadline {
        let line = timeout(Duration::from_secs(3), lines.next_line()).await;
        match line {
            Ok(Ok(Some(line))) => {
                if let Some(url) = extract_public_url(&line) {
                    return Ok(url);
                }
            }
            Ok(Ok(None)) => break,
            Ok(Err(err)) => return Err(NgrokError::Start(err.to_string())),
            Err(_) => {}
        }
    }

    Err(NgrokError::Start(
        "ngrok did not produce a public URL within 15s".to_string(),
    ))
}

fn extract_public_url(line: &str) -> Option<String> {
    let idx = line.find("url=https://")?;
    let url_start = idx + "url=".len();
    let url_part = &line[url_start..];
    let end = url_part
        .find(|character: char| character.is_whitespace())
        .unwrap_or(url_part.len());
    Some(url_part[..end].trim_end_matches('/').to_string())
}

pub fn ensure_ngrok(runtime_root: &Path) -> Result<PathBuf, NgrokError> {
    if let Some(path) = find_on_path("ngrok") {
        return Ok(path);
    }

    let local_ngrok = runtime_root.join("bin").join("ngrok");
    if local_ngrok.exists() {
        return Ok(local_ngrok);
    }

    install_ngrok(runtime_root)?;
    Ok(local_ngrok)
}

fn install_ngrok(runtime_root: &Path) -> Result<(), NgrokError> {
    let package = NgrokPackage::for_current_platform()?;
    let bin_dir = runtime_root.join("bin");
    fs::create_dir_all(&bin_dir).map_err(|err| NgrokError::Install(err.to_string()))?;

    let archive = runtime_root.join(package.file_name);
    run(Command::new("curl")
        .args(["-fsSL", package.url, "-o"])
        .arg(&archive))?;
    match package.kind {
        ArchiveKind::Tgz => {
            run(Command::new("tar")
                .args(["-xzf"])
                .arg(&archive)
                .args(["-C"])
                .arg(&bin_dir))?;
        }
        ArchiveKind::Zip => {
            run(Command::new("unzip")
                .args(["-o"])
                .arg(&archive)
                .args(["-d"])
                .arg(&bin_dir))?;
        }
    }
    fs::remove_file(&archive).ok();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = bin_dir.join("ngrok");
        let mut permissions = fs::metadata(&path)
            .map_err(|err| NgrokError::Install(err.to_string()))?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)
            .map_err(|err| NgrokError::Install(err.to_string()))?;
    }

    Ok(())
}

fn run(command: &mut Command) -> Result<(), NgrokError> {
    let output = command
        .output()
        .map_err(|err| NgrokError::Install(err.to_string()))?;
    if output.status.success() {
        return Ok(());
    }

    Err(NgrokError::Install(
        String::from_utf8(output.stderr)
            .unwrap_or_else(|_| "command failed with non-UTF8 stderr".to_string()),
    ))
}

fn find_on_path(binary: &str) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    env::split_paths(&paths)
        .map(|path| path.join(binary))
        .find(|path| path.is_file())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArchiveKind {
    Tgz,
    Zip,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NgrokPackage {
    url: &'static str,
    file_name: &'static str,
    kind: ArchiveKind,
}

impl NgrokPackage {
    fn for_current_platform() -> Result<Self, NgrokError> {
        Self::for_platform(env::consts::OS, env::consts::ARCH)
    }

    fn for_platform(os: &str, arch: &str) -> Result<Self, NgrokError> {
        match (os, arch) {
            ("linux", "x86_64") => Ok(Self {
                url: "https://bin.equinox.io/c/bNyj1mQVY4c/ngrok-v3-stable-linux-amd64.tgz",
                file_name: "ngrok-v3-stable-linux-amd64.tgz",
                kind: ArchiveKind::Tgz,
            }),
            ("linux", "aarch64") => Ok(Self {
                url: "https://bin.equinox.io/c/bNyj1mQVY4c/ngrok-v3-stable-linux-arm64.tgz",
                file_name: "ngrok-v3-stable-linux-arm64.tgz",
                kind: ArchiveKind::Tgz,
            }),
            ("macos", "x86_64") => Ok(Self {
                url: "https://bin.equinox.io/c/bNyj1mQVY4c/ngrok-v3-stable-darwin-amd64.zip",
                file_name: "ngrok-v3-stable-darwin-amd64.zip",
                kind: ArchiveKind::Zip,
            }),
            ("macos", "aarch64") => Ok(Self {
                url: "https://bin.equinox.io/c/bNyj1mQVY4c/ngrok-v3-stable-darwin-arm64.zip",
                file_name: "ngrok-v3-stable-darwin-arm64.zip",
                kind: ArchiveKind::Zip,
            }),
            _ => Err(NgrokError::UnsupportedPlatform(format!("{os}/{arch}"))),
        }
    }
}
