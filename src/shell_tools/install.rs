use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use super::{
    InstallMethod, MANIFEST_FILENAME, ShellToolManifest,
    process::{ProcessRequest, ProcessRunner},
};

const INSTALL_TIMEOUT: Duration = Duration::from_secs(300);

pub async fn install(
    manifest: &ShellToolManifest,
    source_dir: &Path,
    tool_dir: &Path,
    runner: &ProcessRunner,
) -> Result<(), String> {
    std::fs::create_dir_all(tool_dir.join("bin")).map_err(|error| error.to_string())?;
    match &manifest.install {
        InstallMethod::Preinstalled { name } => install_preinstalled(manifest, name, tool_dir)?,
        InstallMethod::Curl { url, run_with_sh } => {
            install_download(manifest, "curl", url, *run_with_sh, tool_dir, runner).await?
        }
        InstallMethod::Wget { url, run_with_sh } => {
            install_download(manifest, "wget", url, *run_with_sh, tool_dir, runner).await?
        }
        InstallMethod::Npm { package } => install_npm(manifest, package, tool_dir, runner).await?,
    }
    link_installed_commands(manifest, tool_dir)?;
    copy_skill(manifest, source_dir, tool_dir)?;
    let json = serde_json::to_string_pretty(manifest).map_err(|error| error.to_string())?;
    std::fs::write(tool_dir.join(MANIFEST_FILENAME), format!("{json}\n"))
        .map_err(|error| error.to_string())?;
    make_package_read_only(tool_dir)
}

#[cfg(unix)]
fn make_package_read_only(tool_dir: &Path) -> Result<(), String> {
    for entry in std::fs::read_dir(tool_dir).map_err(|error| error.to_string())? {
        let path = entry.map_err(|error| error.to_string())?.path();
        remove_write_permissions(&path)?;
    }
    remove_own_write_permissions(tool_dir)
}

#[cfg(unix)]
fn remove_write_permissions(path: &Path) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path).map_err(|error| error.to_string())? {
            remove_write_permissions(&entry.map_err(|error| error.to_string())?.path())?;
        }
    }
    remove_own_write_permissions(path)
}

#[cfg(unix)]
fn remove_own_write_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;

    let mut permissions = std::fs::metadata(path)
        .map_err(|error| error.to_string())?
        .permissions();
    permissions.set_mode(permissions.mode() & !0o222);
    std::fs::set_permissions(path, permissions).map_err(|error| error.to_string())
}

#[cfg(not(unix))]
fn make_package_read_only(_tool_dir: &Path) -> Result<(), String> {
    Err("shell tools are supported only on Unix platforms".to_string())
}

fn install_preinstalled(
    manifest: &ShellToolManifest,
    primary: &str,
    tool_dir: &Path,
) -> Result<(), String> {
    find_command(primary)?;
    for command in &manifest.commands {
        let source = find_command(&command.name)?;
        link(&source, &tool_dir.join("bin").join(&command.name), tool_dir)?;
    }
    Ok(())
}

async fn install_download(
    manifest: &ShellToolManifest,
    downloader: &str,
    url: &str,
    run_with_sh: bool,
    tool_dir: &Path,
    runner: &ProcessRunner,
) -> Result<(), String> {
    let executable = find_command(downloader)?;
    let destination = if run_with_sh {
        tool_dir.join("tmp/install.sh")
    } else {
        let [command] = manifest.commands.as_slice() else {
            return Err("a direct binary download must expose exactly one command".to_string());
        };
        tool_dir.join("bin").join(&command.name)
    };
    let relative_destination = destination
        .strip_prefix(tool_dir)
        .expect("installer destination is in tool directory")
        .to_string_lossy()
        .to_string();
    let args = match downloader {
        "curl" => vec![
            "-fsSL".to_string(),
            url.to_string(),
            "-o".to_string(),
            relative_destination,
        ],
        "wget" => vec![
            "-q".to_string(),
            "-O".to_string(),
            relative_destination,
            url.to_string(),
        ],
        _ => return Err(format!("unsupported downloader: {downloader}")),
    };
    run(runner, executable, args, Vec::new()).await?;
    if run_with_sh {
        run(
            runner,
            find_command("sh")?,
            vec!["tmp/install.sh".to_string()],
            vec![("PREFIX".to_string(), ".".to_string())],
        )
        .await?;
    } else {
        make_executable(&destination)?;
    }
    Ok(())
}

async fn install_npm(
    _manifest: &ShellToolManifest,
    package: &str,
    _tool_dir: &Path,
    runner: &ProcessRunner,
) -> Result<(), String> {
    run(
        runner,
        find_command("npm")?,
        vec![
            "install".to_string(),
            "--prefix".to_string(),
            ".".to_string(),
            package.to_string(),
        ],
        Vec::new(),
    )
    .await
}

async fn run(
    runner: &ProcessRunner,
    executable: PathBuf,
    args: Vec<String>,
    env: Vec<(String, String)>,
) -> Result<(), String> {
    let output = runner
        .run(ProcessRequest {
            executable,
            args,
            env,
            working_dir: None,
            timeout: INSTALL_TIMEOUT,
        })
        .await?;
    if output.exit_code == Some(0) {
        return Ok(());
    }
    Err(format!(
        "installer exited with {:?}: {}",
        output.exit_code,
        output.stderr.trim()
    ))
}

fn link_installed_commands(manifest: &ShellToolManifest, tool_dir: &Path) -> Result<(), String> {
    for command in &manifest.commands {
        let destination = tool_dir.join("bin").join(&command.name);
        if executable_file(&destination) {
            continue;
        }
        let candidates = [
            tool_dir.join("node_modules/.bin").join(&command.name),
            tool_dir.join(".local/bin").join(&command.name),
            tool_dir.join(&command.name),
        ];
        let source = candidates
            .into_iter()
            .find(|candidate| executable_file(candidate))
            .map(Ok)
            .unwrap_or_else(|| find_command(&command.name))?;
        link(&source, &destination, tool_dir)?;
    }
    Ok(())
}

fn copy_skill(
    manifest: &ShellToolManifest,
    source_dir: &Path,
    tool_dir: &Path,
) -> Result<(), String> {
    let Some(skill) = &manifest.skill else {
        return Ok(());
    };
    let source = source_dir.join(skill);
    let destination = tool_dir.join(skill);
    let parent = destination
        .parent()
        .ok_or_else(|| format!("skill has no parent: {}", destination.display()))?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    std::fs::copy(&source, &destination)
        .map_err(|error| format!("failed to copy {}: {error}", source.display()))?;
    Ok(())
}

fn find_command(name: &str) -> Result<PathBuf, String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| executable_file(candidate))
        .ok_or_else(|| format!("required installer command '{name}' is not available"))
}

fn executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    true
}

fn make_executable(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path)
            .map_err(|error| error.to_string())?
            .permissions();
        permissions.set_mode(permissions.mode() | 0o700);
        std::fs::set_permissions(path, permissions).map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(unix)]
fn link(source: &Path, destination: &Path, tool_dir: &Path) -> Result<(), String> {
    if destination.symlink_metadata().is_ok() {
        std::fs::remove_file(destination).map_err(|error| error.to_string())?;
    }
    let target = source
        .strip_prefix(tool_dir)
        .map(|relative| Path::new("..").join(relative))
        .unwrap_or_else(|_| source.to_path_buf());
    std::os::unix::fs::symlink(target, destination).map_err(|error| error.to_string())
}

#[cfg(not(unix))]
fn link(_source: &Path, _destination: &Path, _tool_dir: &Path) -> Result<(), String> {
    Err("shell tools are supported only on Unix platforms".to_string())
}
