use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use crate::{
    channels,
    config::RuntimeLayout,
    secrets::Secrets,
    tools::setup::{
        install_channel::{ListChannelsTool, NGROK_SECRET},
        install_common::{
            SetupConfig, SetupSecret, config_statuses, discover_package_dir_names, generate_secret,
            locate_config_template, secret_statuses, write_package_artifacts,
        },
    },
};

pub(super) const BUILTIN_CHANNELS: &[&str] = &["tui"];

#[derive(Clone)]
pub(super) struct ChannelPackage {
    pub(super) name: String,
    pub(super) display_name: String,
    pub(super) wasm_path: PathBuf,
    pub(super) config_template_path: PathBuf,
    pub(super) manifest: ChannelInstallManifest,
}

#[derive(Clone, serde::Deserialize)]
pub(super) struct ChannelInstallManifest {
    pub(super) name: String,
    #[serde(default)]
    pub(super) display_name: Option<String>,
    #[serde(default)]
    pub(super) external: bool,
    #[serde(default)]
    pub(super) secrets: Vec<SetupSecret>,
    #[serde(default)]
    pub(super) config: Vec<SetupConfig>,
    #[serde(default)]
    pub(super) setup_url: Option<String>,
}

impl ListChannelsTool {
    pub(super) async fn execute(&self) -> Result<serde_json::Value, String> {
        let installed = installed_channel_names(&self.layout)?
            .into_iter()
            .collect::<BTreeSet<_>>();
        let mut package_names = BTreeSet::new();
        let mut channel_rows = Vec::new();

        for package in discover_channel_packages(&self.layout) {
            let package = package?;
            let installed = installed.contains(&package.name);
            package_names.insert(package.name.clone());
            channel_rows.push(serde_json::json!({
                "name": package.name,
                "display_name": package.display_name,
                "installed": installed,
                "installable": !installed,
                "external": package.manifest.external,
                "required_secrets": secret_statuses(required_secrets(&package, &self.secrets), &self.secrets, true),
                "required_config": config_statuses(required_config(&package)),
            }));
        }

        for name in installed.difference(&package_names) {
            channel_rows.push(serde_json::json!({
                "name": name,
                "installed": true,
                "installable": false,
                "external": null,
                "required_secrets": [],
                "required_config": [],
            }));
        }

        channel_rows.sort_by(|left, right| {
            left["name"]
                .as_str()
                .unwrap_or_default()
                .cmp(right["name"].as_str().unwrap_or_default())
        });

        Ok(serde_json::json!({
            "ok": true,
            "channels": channel_rows,
        }))
    }
}

pub(super) fn normalize_channel_name(name: &str) -> Result<String, String> {
    let name = name.trim().to_ascii_lowercase();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        return Err(
            "channel name must contain only lowercase letters, digits, '_' or '-'".to_string(),
        );
    }
    Ok(name)
}

pub(super) fn locate_channel_package(
    layout: &RuntimeLayout,
    name: &str,
) -> Result<ChannelPackage, String> {
    let source_dir = channel_source_dirs(layout)
        .into_iter()
        .map(|dir| dir.join(name))
        .find(|dir| dir.exists())
        .ok_or_else(|| format!("unknown installable channel: {name}"))?;
    let manifest_path = source_dir.join(format!("{name}.install.json"));
    if !manifest_path.exists() {
        return Err(format!(
            "channel '{name}' is missing {}",
            manifest_path.display()
        ));
    }
    let manifest = parse_install_manifest(
        &std::fs::read_to_string(&manifest_path).map_err(|err| err.to_string())?,
    )?;
    if manifest.name != name {
        return Err(format!(
            "install manifest declares channel '{}' but package is '{name}'",
            manifest.name
        ));
    }
    let wasm_path = locate_wasm(&source_dir, name)?;
    let config_template_path = locate_config_template(&source_dir, name, "channel")?;
    let display_name = manifest
        .display_name
        .clone()
        .unwrap_or_else(|| format!("{name} channel"));
    Ok(ChannelPackage {
        name: name.to_string(),
        display_name,
        wasm_path,
        config_template_path,
        manifest,
    })
}

fn discover_channel_packages(layout: &RuntimeLayout) -> Vec<Result<ChannelPackage, String>> {
    discover_package_dir_names(channel_source_dirs(layout))
        .into_iter()
        .map(|name| locate_channel_package(layout, &name))
        .collect()
}

fn installed_channel_names(layout: &RuntimeLayout) -> Result<Vec<String>, String> {
    let mut names = BUILTIN_CHANNELS
        .iter()
        .map(|name| name.to_string())
        .collect::<Vec<_>>();
    names.extend(
        channels::config::discover(layout)
            .map_err(|err| err.to_string())?
            .into_iter()
            .map(|channel| channel.name),
    );
    names.sort();
    names.dedup();
    Ok(names)
}

fn channel_source_dirs(layout: &RuntimeLayout) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(path) = std::env::var("IRONPAW_CHANNELS_SRC") {
        dirs.push(PathBuf::from(path));
    }
    dirs.push(layout.root().join("channel-store"));
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    dirs.push(manifest_dir.join("channels-src"));
    dirs
}

fn locate_wasm(source_dir: &Path, name: &str) -> Result<PathBuf, String> {
    let flat = source_dir.join(format!("{name}.wasm"));
    if flat.exists() {
        return Ok(flat);
    }
    let built = source_dir
        .join("target")
        .join("wasm32-wasip2")
        .join("release")
        .join(format!("{}_channel.wasm", name.replace('-', "_")));
    if built.exists() {
        return Ok(built);
    }
    Err(format!(
        "channel '{name}' has no built WASM artifact in {}",
        source_dir.display()
    ))
}

pub(super) fn parse_install_manifest(json: &str) -> Result<ChannelInstallManifest, String> {
    serde_json::from_str(json).map_err(|err| err.to_string())
}

pub(super) fn required_secrets(
    package: &ChannelPackage,
    secrets_store: &Secrets,
) -> Vec<SetupSecret> {
    let mut secrets = Vec::new();
    if package.manifest.external && !secrets_store.exists(NGROK_SECRET) {
        secrets.push(SetupSecret {
            name: NGROK_SECRET.to_string(),
            prompt: Some(
                "Paste your ngrok auth token so Warden can expose webhook channels.".to_string(),
            ),
            optional: false,
            auto_generate: None,
        });
    }
    secrets.extend(package.manifest.secrets.clone());
    secrets
}

pub(super) fn required_config(package: &ChannelPackage) -> Vec<SetupConfig> {
    package.manifest.config.clone()
}

pub(super) fn secret_prompt(package: &ChannelPackage, secret: &SetupSecret) -> String {
    let mut prompt = format!(
        "{}: {}",
        package.display_name,
        secret.prompt.as_deref().unwrap_or(&secret.name)
    );
    if secret.name == NGROK_SECRET {
        prompt.push_str(" You can find it in your ngrok dashboard.");
    } else if let Some(setup_url) = &package.manifest.setup_url {
        prompt.push_str(&format!(" Setup: {setup_url}"));
    }
    prompt
}

pub(super) fn verification_code() -> String {
    generate_secret(3).to_ascii_uppercase()
}

pub(super) fn set_channel_owner_id(
    layout: &RuntimeLayout,
    package: &ChannelPackage,
    owner_id: &str,
) -> Result<(), String> {
    let config_path = layout
        .channels_dir()
        .join(format!("{}.config.json", package.name));
    let mut config: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&config_path)
            .map_err(|err| format!("could not read {}: {err}", config_path.display()))?,
    )
    .map_err(|err| format!("could not parse {}: {err}", config_path.display()))?;
    config["owner_id"] = serde_json::Value::String(owner_id.to_string());
    std::fs::write(
        &config_path,
        serde_json::to_string_pretty(&config).map_err(|err| err.to_string())?,
    )
    .map_err(|err| err.to_string())
}

pub(super) fn write_package(
    layout: &RuntimeLayout,
    package: &ChannelPackage,
    config_values: &BTreeMap<String, serde_json::Value>,
) -> Result<(), String> {
    write_package_artifacts(
        "channel",
        &layout.channels_dir(),
        &package.name,
        &package.wasm_path,
        &package.config_template_path,
        config_values,
        false,
    )?;
    Ok(())
}

pub(super) fn installed_package_exists(layout: &RuntimeLayout, package: &ChannelPackage) -> bool {
    let channels_dir = layout.channels_dir();
    channels_dir.join(format!("{}.wasm", package.name)).exists()
        || channels_dir
            .join(format!("{}.config.json", package.name))
            .exists()
}

pub(super) fn remove_installed_package(layout: &RuntimeLayout, package: &ChannelPackage) {
    remove_channel_files(layout, &package.name);
}

pub(super) fn remove_channel_files(layout: &RuntimeLayout, name: &str) -> bool {
    let channels_dir = layout.channels_dir();
    let mut removed = false;
    for path in [
        channels_dir.join(format!("{name}.wasm")),
        channels_dir.join(format!("{name}.config.json")),
    ] {
        match std::fs::remove_file(&path) {
            Ok(()) => removed = true,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                eprintln!("failed to remove channel file {}: {err}", path.display());
            }
        }
    }
    removed
}
