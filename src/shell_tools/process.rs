use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

use crate::network::proxy::ConnectProxy;

const OUTPUT_LIMIT: usize = 1024 * 1024;
const SAFE_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

#[derive(Clone, Copy)]
enum Isolation {
    #[cfg(debug_assertions)]
    Portable,
    Linux,
}

#[derive(Clone, Copy)]
pub(super) enum ProcessMode {
    Install,
    Execute,
}

pub struct ProcessRequest {
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub working_dir: Option<PathBuf>,
    pub timeout: Duration,
}

#[derive(Debug, serde::Serialize)]
pub struct ProcessOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub duration_ms: u64,
}

pub struct ProcessRunner {
    tool_dir: PathBuf,
    workspace: PathBuf,
    allowed_hosts: Vec<String>,
    isolation: Isolation,
    mode: ProcessMode,
}

impl ProcessRunner {
    pub(super) fn new(
        tool_dir: PathBuf,
        allowed_hosts: Vec<String>,
        mode: ProcessMode,
    ) -> Result<Self, String> {
        if !cfg!(target_os = "linux") {
            return Err("Linux shell isolation is unavailable on this platform".to_string());
        }
        Self::with_isolation(tool_dir, allowed_hosts, Isolation::Linux, mode)
    }

    #[cfg(debug_assertions)]
    pub(super) fn portable(
        tool_dir: PathBuf,
        allowed_hosts: Vec<String>,
        mode: ProcessMode,
    ) -> Result<Self, String> {
        Self::with_isolation(tool_dir, allowed_hosts, Isolation::Portable, mode)
    }

    fn with_isolation(
        tool_dir: PathBuf,
        allowed_hosts: Vec<String>,
        isolation: Isolation,
        mode: ProcessMode,
    ) -> Result<Self, String> {
        std::fs::create_dir_all(&tool_dir).map_err(|error| error.to_string())?;
        let workspace = match mode {
            ProcessMode::Install => tool_dir.clone(),
            ProcessMode::Execute => tool_dir.join("workspace"),
        };
        if let ProcessMode::Execute = mode {
            match std::fs::symlink_metadata(&workspace) {
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                    return Err("shell tool workspace must be a directory".to_string());
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    std::fs::create_dir(&workspace).map_err(|error| error.to_string())?;
                }
                Err(error) => return Err(error.to_string()),
            }
        }
        std::fs::create_dir_all(workspace.join("tmp")).map_err(|error| error.to_string())?;
        let tool_dir = tool_dir
            .canonicalize()
            .map_err(|error| format!("failed to resolve tool directory: {error}"))?;
        let workspace = workspace
            .canonicalize()
            .map_err(|error| format!("failed to resolve tool workspace: {error}"))?;
        if !workspace.starts_with(&tool_dir) {
            return Err("shell tool workspace escapes the tool directory".to_string());
        }
        Ok(Self {
            tool_dir,
            workspace,
            allowed_hosts,
            isolation,
            mode,
        })
    }

    pub async fn run(&self, request: ProcessRequest) -> Result<ProcessOutput, String> {
        let working_dir = self.resolve_working_dir(request.working_dir.as_deref())?;
        let executable = request
            .executable
            .canonicalize()
            .map_err(|error| format!("failed to resolve executable: {error}"))?;
        if !executable.is_file() {
            return Err(format!(
                "executable is not a regular file: {}",
                executable.display()
            ));
        }

        let (proxy, mut command, proxy_url) = match self.isolation {
            #[cfg(debug_assertions)]
            Isolation::Portable => {
                let proxy = ConnectProxy::shell_tcp(self.allowed_hosts.clone()).await?;
                let url = format!("http://{}", proxy.tcp_address());
                (proxy, direct_command(&executable, &request.args), Some(url))
            }
            Isolation::Linux => {
                let socket_path = std::env::temp_dir()
                    .join(format!("ironpaw-proxy-{}.sock", uuid::Uuid::new_v4()));
                let proxy =
                    ConnectProxy::shell_unix(socket_path, self.allowed_hosts.clone()).await?;
                let command = linux_command(
                    &self.tool_dir,
                    &self.workspace,
                    self.mode,
                    &executable,
                    &request.args,
                    proxy.socket_path(),
                    &working_dir,
                )?;
                (proxy, command, None::<String>)
            }
        };
        configure_process(
            &mut command,
            &self.workspace,
            &working_dir,
            proxy_url.as_deref(),
            &request.env,
            match self.isolation {
                #[cfg(debug_assertions)]
                Isolation::Portable => self.tool_dir.join("bin"),
                Isolation::Linux => match self.mode {
                    ProcessMode::Install => PathBuf::from("/workspace/bin"),
                    ProcessMode::Execute => PathBuf::from("/tool/bin"),
                },
            },
        );
        let started = Instant::now();
        let mut child = command
            .spawn()
            .map_err(|error| format!("failed to start command: {error}"))?;
        let process_group = child
            .id()
            .ok_or_else(|| "spawned command has no process id".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "command stdout was not captured".to_string())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "command stderr was not captured".to_string())?;
        let stdout_task = tokio::spawn(capture(stdout));
        let stderr_task = tokio::spawn(capture(stderr));

        let status = match tokio::time::timeout(request.timeout, child.wait()).await {
            Ok(status) => status.map_err(|error| format!("failed to wait for command: {error}"))?,
            Err(_) => {
                kill_process_group(process_group);
                child
                    .wait()
                    .await
                    .map_err(|error| format!("failed to reap timed out command: {error}"))?;
                kill_process_group(process_group);
                return Err(format!("command timed out after {:?}", request.timeout));
            }
        };
        drop(proxy);
        kill_process_group(process_group);
        let (stdout, stdout_truncated) = stdout_task
            .await
            .map_err(|error| format!("stdout capture failed: {error}"))?;
        let (stderr, stderr_truncated) = stderr_task
            .await
            .map_err(|error| format!("stderr capture failed: {error}"))?;
        Ok(ProcessOutput {
            stdout,
            stderr,
            exit_code: status.code(),
            stdout_truncated,
            stderr_truncated,
            duration_ms: started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        })
    }

    fn resolve_working_dir(&self, relative: Option<&Path>) -> Result<PathBuf, String> {
        let Some(relative) = relative else {
            return Ok(self.workspace.clone());
        };
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err("shell working directory must be a safe relative path".to_string());
        }
        let working_dir = self
            .workspace
            .join(relative)
            .canonicalize()
            .map_err(|error| format!("failed to resolve shell working directory: {error}"))?;
        if !working_dir.starts_with(&self.workspace) || !working_dir.is_dir() {
            return Err("shell working directory escapes the tool workspace".to_string());
        }
        Ok(working_dir)
    }
}

#[cfg(debug_assertions)]
fn direct_command(executable: &Path, args: &[String]) -> Command {
    let mut command = Command::new(executable);
    command.args(args);
    command
}

fn linux_command(
    tool_dir: &Path,
    workspace: &Path,
    mode: ProcessMode,
    executable: &Path,
    args: &[String],
    proxy_socket: &Path,
    working_dir: &Path,
) -> Result<Command, String> {
    if !cfg!(target_os = "linux") {
        return Err("Linux shell isolation is unavailable on this platform".to_string());
    }
    let bubblewrap = ["/usr/bin/bwrap", "/bin/bwrap"]
        .into_iter()
        .map(Path::new)
        .find(|path| path.is_file())
        .ok_or_else(|| "bubblewrap is required for Linux shell isolation".to_string())?;
    let bridge = std::env::current_exe()
        .map_err(|error| format!("failed to resolve Ironpaw executable: {error}"))?
        .canonicalize()
        .map_err(|error| format!("failed to resolve Ironpaw executable: {error}"))?;
    let tool_executable = executable
        .strip_prefix(tool_dir)
        .ok()
        .map(|relative| match mode {
            ProcessMode::Install => Path::new("/workspace").join(relative),
            ProcessMode::Execute => Path::new("/tool").join(relative),
        });
    let system_executable = ["/usr", "/bin", "/lib", "/lib64"]
        .into_iter()
        .any(|root| executable.starts_with(root))
        .then(|| executable.to_path_buf());
    let sandbox_executable = tool_executable
        .or(system_executable)
        .unwrap_or_else(|| PathBuf::from("/ironpaw/bin/command"));
    let relative_working_dir = working_dir
        .strip_prefix(workspace)
        .map_err(|_| "shell working directory escapes the tool workspace".to_string())?;
    let sandbox_working_dir = Path::new("/workspace").join(relative_working_dir);
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
    ]);
    match mode {
        ProcessMode::Install => {
            command.args(["--dir", "/workspace", "--bind"]);
            command.arg(tool_dir).arg("/workspace");
        }
        ProcessMode::Execute => {
            command.args(["--dir", "/tool", "--ro-bind"]);
            command.arg(tool_dir).arg("/tool");
            command.args(["--dir", "/workspace", "--bind"]);
            command.arg(workspace).arg("/workspace");
        }
    }
    for path in ["/usr", "/bin", "/lib", "/lib64"] {
        if Path::new(path).exists() {
            command.arg("--ro-bind").arg(path).arg(path);
        }
    }
    command.args(["--dir", "/etc"]);
    for path in ["/etc/ssl", "/etc/pki"] {
        if Path::new(path).exists() {
            command.arg("--ro-bind").arg(path).arg(path);
        }
    }
    command.args(["--dir", "/ironpaw", "--dir", "/ironpaw/bin"]);
    if sandbox_executable == Path::new("/ironpaw/bin/command") {
        command.arg("--ro-bind");
        command.arg(executable).arg("/ironpaw/bin/command");
    }
    command.args(["--ro-bind"]);
    command.arg(bridge).arg("/ironpaw/bin/proxy-bridge");
    command.args(["--dir", "/ironpaw/run", "--bind"]);
    command.arg(proxy_socket).arg("/ironpaw/run/proxy.sock");
    command.arg("--chdir").arg(sandbox_working_dir);
    command.args([
        "--setenv",
        "HOME",
        "/workspace",
        "--setenv",
        "TMPDIR",
        "/tmp",
        "--setenv",
        "PATH",
        SAFE_PATH,
        "--",
        "/ironpaw/bin/proxy-bridge",
        crate::network::proxy_bridge::ARGUMENT,
        "/ironpaw/run/proxy.sock",
    ]);
    command.arg(sandbox_executable);
    command.args(args);
    Ok(command)
}

fn configure_process(
    command: &mut Command,
    workspace: &Path,
    working_dir: &Path,
    proxy_url: Option<&str>,
    env: &[(String, String)],
    tool_bin: PathBuf,
) {
    command
        .current_dir(working_dir)
        .env_clear()
        .env("HOME", workspace)
        .env("TMPDIR", workspace.join("tmp"))
        .env("PATH", SAFE_PATH)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command.envs(env.iter().map(|(name, value)| (name, value)));
    command.env("IRONPAW_TOOL_BIN", tool_bin);
    if let Some(proxy_url) = proxy_url {
        command
            .env("HTTP_PROXY", proxy_url)
            .env("HTTPS_PROXY", proxy_url)
            .env("ALL_PROXY", proxy_url)
            .env("http_proxy", proxy_url)
            .env("https_proxy", proxy_url)
            .env("all_proxy", proxy_url);
    }
    #[cfg(unix)]
    command.process_group(0);
}

async fn capture(mut reader: impl AsyncRead + Unpin) -> (String, bool) {
    let mut kept = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    while let Ok(read) = reader.read(&mut buffer).await {
        if read == 0 {
            break;
        }
        let remaining = OUTPUT_LIMIT.saturating_sub(kept.len());
        kept.extend_from_slice(&buffer[..read.min(remaining)]);
        truncated |= read > remaining;
    }
    (String::from_utf8_lossy(&kept).into_owned(), truncated)
}

#[cfg(unix)]
fn kill_process_group(process_group: u32) {
    // SAFETY: A negative PID addresses the child process group created at spawn.
    unsafe {
        libc::kill(-(process_group as i32), libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_process_group(_process_group: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("ironpaw-shell-{}", uuid::Uuid::new_v4()))
    }

    #[tokio::test]
    async fn portable_runner_uses_private_directory_and_scrubbed_environment() {
        let tool_dir = temp_dir();
        let runner =
            ProcessRunner::portable(tool_dir.clone(), Vec::new(), ProcessMode::Execute).unwrap();
        let output = runner
            .run(ProcessRequest {
                executable: PathBuf::from("/bin/sh"),
                args: vec![
                    "-c".to_string(),
                    "printf '%s\\n%s\\n%s' \"$PWD\" \"$HOME\" \"${IRONPAW_TEST_UNSAFE-unset}\""
                        .to_string(),
                ],
                env: Vec::new(),
                working_dir: None,
                timeout: Duration::from_secs(5),
            })
            .await
            .unwrap();

        let canonical = tool_dir.join("workspace").canonicalize().unwrap();
        assert_eq!(
            output.stdout,
            format!("{}\n{}\nunset", canonical.display(), canonical.display())
        );
        assert_eq!(output.exit_code, Some(0));
        assert!(output.stderr.is_empty());
        assert!(!output.stdout_truncated);
        assert!(!output.stderr_truncated);
        assert!(output.duration_ms <= 5_000);
        std::fs::remove_dir_all(tool_dir).unwrap();
    }

    #[tokio::test]
    async fn portable_runner_enforces_timeout() {
        let tool_dir = temp_dir();
        let runner =
            ProcessRunner::portable(tool_dir.clone(), Vec::new(), ProcessMode::Execute).unwrap();
        let error = runner
            .run(ProcessRequest {
                executable: PathBuf::from("/bin/sh"),
                args: vec!["-c".to_string(), "sleep 10".to_string()],
                env: Vec::new(),
                working_dir: None,
                timeout: Duration::from_millis(20),
            })
            .await
            .unwrap_err();

        assert_eq!(error, "command timed out after 20ms");
        std::fs::remove_dir_all(tool_dir).unwrap();
    }

    #[tokio::test]
    async fn portable_runner_confines_relative_working_directory() {
        let tool_dir = temp_dir();
        std::fs::create_dir_all(tool_dir.join("workspace/repo")).unwrap();
        let runner =
            ProcessRunner::portable(tool_dir.clone(), Vec::new(), ProcessMode::Execute).unwrap();
        let output = runner
            .run(ProcessRequest {
                executable: PathBuf::from("/bin/pwd"),
                args: Vec::new(),
                env: Vec::new(),
                working_dir: Some(PathBuf::from("repo")),
                timeout: Duration::from_secs(5),
            })
            .await
            .unwrap();
        assert_eq!(
            output.stdout.trim(),
            tool_dir
                .join("workspace/repo")
                .canonicalize()
                .unwrap()
                .to_string_lossy()
        );
        let error = runner
            .run(ProcessRequest {
                executable: PathBuf::from("/bin/pwd"),
                args: Vec::new(),
                env: Vec::new(),
                working_dir: Some(PathBuf::from("../")),
                timeout: Duration::from_secs(5),
            })
            .await
            .unwrap_err();
        assert_eq!(
            error,
            "shell working directory must be a safe relative path"
        );
        std::fs::remove_dir_all(tool_dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn runner_rejects_a_workspace_symlink() {
        let tool_dir = temp_dir();
        let outside = temp_dir();
        std::fs::create_dir_all(&tool_dir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, tool_dir.join("workspace")).unwrap();

        let error = ProcessRunner::portable(tool_dir.clone(), Vec::new(), ProcessMode::Execute)
            .err()
            .unwrap();

        assert_eq!(error, "shell tool workspace must be a directory");
        assert!(!outside.join("tmp").exists());
        std::fs::remove_dir_all(tool_dir).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn linux_isolation_fails_closed_on_other_platforms() {
        if cfg!(target_os = "linux") {
            return;
        }
        let error = linux_command(
            Path::new("/tmp"),
            Path::new("/tmp/workspace"),
            ProcessMode::Execute,
            Path::new("/bin/sh"),
            &[],
            Path::new("/tmp/proxy.sock"),
            Path::new("/tmp"),
        )
        .unwrap_err();
        assert_eq!(
            error,
            "Linux shell isolation is unavailable on this platform"
        );
    }
}
