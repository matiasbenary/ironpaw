use std::{fs, io::Cursor, path::PathBuf};

use crate::{config::RuntimeLayout, network::public::PublicHttp};

const CFT_STABLE: &str = "https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions-with-downloads.json";

pub(super) async fn ensure_chrome(layout: RuntimeLayout) -> Result<PathBuf, String> {
    tokio::task::spawn_blocking(move || install_chrome(&layout))
        .await
        .map_err(|err| format!("browser installer task failed: {err}"))?
}

fn install_chrome(layout: &RuntimeLayout) -> Result<PathBuf, String> {
    fs::create_dir_all(layout.bin_dir()).map_err(|e| e.to_string())?;
    let metadata = PublicHttp::get_blocking(CFT_STABLE, 1_000_000)?;
    let metadata: serde_json::Value =
        serde_json::from_slice(&metadata.body).map_err(|e| e.to_string())?;
    let platform = chrome_platform();
    let version = metadata["channels"]["Stable"]["version"]
        .as_str()
        .ok_or_else(|| "Chrome for Testing response has no Stable version".to_string())?;
    let install_root = layout.bin_dir().join("chrome-headless-shell").join(version);
    let binary = install_root.join(chrome_relative_path());
    if binary.exists() {
        make_executable(&binary)?;
        return Ok(binary);
    }
    let downloads = &metadata["channels"]["Stable"]["downloads"]["chrome-headless-shell"];
    let url = downloads
        .as_array()
        .and_then(|items| items.iter().find(|item| item["platform"] == platform))
        .and_then(|item| item["url"].as_str())
        .ok_or_else(|| format!("Chrome Headless Shell has no Stable download for {platform}"))?;
    let bytes = PublicHttp::get_blocking(url, 500_000_000)?.body;
    let staging = layout
        .bin_dir()
        .join(format!(".chrome-headless-shell-{version}.installing"));
    fs::remove_dir_all(&staging).ok();
    fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| e.to_string())?;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).map_err(|e| e.to_string())?;
        let Some(name) = entry.enclosed_name() else {
            continue;
        };
        let path = staging.join(name);
        if entry.is_dir() {
            fs::create_dir_all(path).map_err(|e| e.to_string())?;
        } else {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let mut out = fs::File::create(path).map_err(|e| e.to_string())?;
            std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
        }
    }
    if let Some(parent) = install_root.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::rename(staging.join(chrome_archive_dir()), &install_root).map_err(|e| e.to_string())?;
    make_executable(&binary)?;
    Ok(binary)
}

fn make_executable(path: &PathBuf) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)
            .map_err(|err| err.to_string())?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).map_err(|err| err.to_string())?;
    }
    Ok(())
}

fn chrome_platform() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux64",
        ("macos", "aarch64") => "mac-arm64",
        ("macos", "x86_64") => "mac-x64",
        _ => "linux64",
    }
}

fn chrome_relative_path() -> &'static str {
    "chrome-headless-shell"
}

fn chrome_archive_dir() -> &'static str {
    match chrome_platform() {
        "mac-arm64" => "chrome-headless-shell-mac-arm64",
        "mac-x64" => "chrome-headless-shell-mac-x64",
        _ => "chrome-headless-shell-linux64",
    }
}
