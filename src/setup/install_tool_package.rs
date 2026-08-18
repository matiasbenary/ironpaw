use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use crate::{
    config::RuntimeLayout,
    shell_tools::ShellToolManifest,
    tools::wasm::{WasmToolConfig, WasmToolManifest},
};

use crate::setup::install_common::{
    SetupConfig, SetupSecret, config_statuses, discover_package_dir_names, locate_config_template,
    secret_statuses,
};

use super::{
    ListToolsTool, install_tool_artifacts::install_package,
    installable_tool::discover_shell_packages,
};

#[derive(Clone)]
pub(super) struct ToolPackage {
    pub(super) name: String,
    pub(super) display_name: String,
    pub(super) wasm_path: PathBuf,
    pub(super) config_template_path: PathBuf,
    pub(super) manifest: ToolInstallManifest,
}

#[derive(Clone, serde::Deserialize)]
pub(super) struct ToolInstallManifest {
    pub(super) name: String,
    #[serde(default)]
    pub(super) display_name: Option<String>,
    #[serde(default)]
    pub(super) description: Option<String>,
    #[serde(default)]
    pub(super) secrets: Vec<SetupSecret>,
    #[serde(default)]
    pub(super) config: Vec<SetupConfig>,
    #[serde(default)]
    pub(super) setup_url: Option<String>,
    #[serde(default)]
    pub(super) config_before_secrets: bool,
    #[serde(default)]
    pub(super) setup_handler: Option<String>,
    #[serde(default)]
    pub(super) accounts: Option<SetupAccounts>,
}

#[derive(Clone, serde::Deserialize)]
pub(super) struct SetupAccounts {
    pub(super) path: String,
    pub(super) identity_path: String,
    pub(super) template: serde_json::Value,
}

pub fn install_packaged_tool(
    layout: &RuntimeLayout,
    name: &str,
) -> Result<WasmToolManifest, String> {
    let name = normalize_tool_name(name)?;
    if let Some(manifest) = installed_tool_manifest(layout, &name)? {
        return Ok(manifest);
    }
    let package = locate_tool_package(layout, &name)?;
    if !required_config(&package).is_empty() {
        return Err(format!(
            "tool '{}' requires interactive config; use install_tool",
            package.name
        ));
    }
    install_package(layout, &package, &BTreeMap::new())
}

pub(super) fn installed_tool_manifest(
    layout: &RuntimeLayout,
    name: &str,
) -> Result<Option<WasmToolManifest>, String> {
    let tools_dir = layout.tools_dir();
    let wasm_path = tools_dir.join(format!("{name}.wasm"));
    let config_path = tools_dir.join(format!("{name}.config.json"));
    if !wasm_path.exists() || !config_path.exists() {
        return Ok(None);
    }
    let config_json = std::fs::read_to_string(&config_path).map_err(|err| err.to_string())?;
    let config: WasmToolConfig =
        serde_json::from_str(&config_json).map_err(|err| err.to_string())?;
    Ok(Some(WasmToolManifest {
        name: name.to_string(),
        wasm_path,
        config_path,
        config_json,
        config,
    }))
}

impl ListToolsTool {
    pub(super) async fn execute(&self) -> Result<serde_json::Value, String> {
        self.inventory()
    }

    fn inventory(&self) -> Result<serde_json::Value, String> {
        let installed = installed_tool_names(&self.layout)?
            .into_iter()
            .collect::<BTreeSet<_>>();
        let mut package_names = BTreeSet::new();
        let mut tool_rows = Vec::new();

        for package in discover_tool_packages(&self.layout) {
            let package = package?;
            let is_installed = installed.contains(&package.name);
            package_names.insert(package.name.clone());
            tool_rows.push(serde_json::json!({
                "name": package.name,
                "kind": "wasm",
                "display_name": package.display_name,
                "description": package.manifest.description,
                "installed": is_installed,
                "installable": !is_installed,
                "required_secrets": secret_statuses(required_secrets(&package), &self.secrets, false),
                "required_config": config_statuses(required_config(&package)),
            }));
        }
        for name in installed.difference(&package_names) {
            tool_rows.push(serde_json::json!({
                "name": name,
                "kind": "wasm",
                "installed": true,
                "installable": false,
                "required_secrets": [],
                "required_config": [],
            }));
        }

        let enabled = crate::tools::state::EnabledTools::new(self.layout.tools_state_path());
        let installed_shell = ShellToolManifest::discover(&self.layout.tools_dir())?;
        let installed_shell_names = installed_shell
            .iter()
            .map(|manifest| manifest.name.as_str())
            .collect::<BTreeSet<_>>();
        for package in discover_shell_packages(&self.layout) {
            let package = package?;
            if !installed_shell_names.contains(package.manifest.name.as_str()) {
                tool_rows.push(shell_inventory_row(
                    &package.manifest,
                    false,
                    false,
                    &self.secrets,
                ));
            }
        }
        for manifest in installed_shell {
            tool_rows.push(shell_inventory_row(
                &manifest,
                true,
                enabled.is_enabled(&manifest.name)?,
                &self.secrets,
            ));
        }
        tool_rows.sort_by(|left, right| {
            left["name"]
                .as_str()
                .unwrap_or_default()
                .cmp(right["name"].as_str().unwrap_or_default())
        });
        Ok(serde_json::json!({ "ok": true, "tools": tool_rows }))
    }
}

fn shell_inventory_row(
    manifest: &ShellToolManifest,
    installed: bool,
    enabled: bool,
    secrets: &crate::secrets::Secrets,
) -> serde_json::Value {
    let required_secrets = manifest
        .secrets
        .iter()
        .map(|secret| {
            serde_json::json!({
                "name": secret.name,
                "env": secret.env,
                "configured": secrets.exists(&secret.name),
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "name": manifest.name,
        "kind": "shell",
        "description": manifest.description,
        "installed": installed,
        "enabled": enabled,
        "installable": !installed,
        "installer": manifest.install,
        "allowed_hosts": manifest.allowed_hosts,
        "required_secrets": required_secrets,
        "required_config": [],
    })
}

pub(super) fn normalize_tool_name(name: &str) -> Result<String, String> {
    let name = name.trim().to_ascii_lowercase().replace('-', "_");
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(
            "tool name must contain only lowercase letters, digits, '_' or '-'".to_string(),
        );
    }
    Ok(name)
}

pub(super) fn locate_tool_package(
    layout: &RuntimeLayout,
    name: &str,
) -> Result<ToolPackage, String> {
    let package_dir_name = name.replace('_', "-");
    let source_dir = tool_source_dirs(layout)
        .into_iter()
        .flat_map(|dir| [dir.join(name), dir.join(&package_dir_name)])
        .find(|dir| dir.exists())
        .ok_or_else(|| format!("unknown installable tool: {name}"))?;
    let actual_dir_name = source_dir
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("invalid tool package path: {}", source_dir.display()))?
        .to_string();
    let manifest_path = source_dir.join(format!("{actual_dir_name}.install.json"));
    if !manifest_path.exists() {
        return Err(format!(
            "tool '{name}' is missing {}",
            manifest_path.display()
        ));
    }
    let manifest: ToolInstallManifest = serde_json::from_str(
        &std::fs::read_to_string(&manifest_path).map_err(|err| err.to_string())?,
    )
    .map_err(|err| err.to_string())?;
    validate_oauth(&manifest)?;
    if normalize_tool_name(&manifest.name)? != name {
        return Err(format!(
            "install manifest declares tool '{}' but package is '{name}'",
            manifest.name
        ));
    }
    let wasm_path = locate_wasm(&source_dir, &actual_dir_name)?;
    let config_template_path = locate_config_template(&source_dir, &actual_dir_name, "tool")?;
    let display_name = manifest
        .display_name
        .clone()
        .unwrap_or_else(|| format!("{name} tool"));
    Ok(ToolPackage {
        name: name.to_string(),
        display_name,
        wasm_path,
        config_template_path,
        manifest,
    })
}

fn validate_oauth(manifest: &ToolInstallManifest) -> Result<(), String> {
    let secret_names = manifest
        .secrets
        .iter()
        .map(|secret| secret.name.as_str())
        .collect::<BTreeSet<_>>();
    let config_paths = manifest
        .config
        .iter()
        .map(|config| config.path.as_str())
        .collect::<BTreeSet<_>>();
    for oauth in manifest
        .config
        .iter()
        .flat_map(|config| &config.choices)
        .filter_map(|choice| choice.oauth.as_ref())
    {
        for (kind, endpoint) in [
            ("authorization", &oauth.authorize_url),
            ("token", &oauth.token_url),
        ] {
            let url = reqwest::Url::parse(endpoint)
                .map_err(|error| format!("invalid OAuth {kind} endpoint: {error}"))?;
            if url.scheme() != "https" || url.host_str().is_none() {
                return Err(format!("OAuth {kind} endpoint must be absolute HTTPS"));
            }
        }
        if let crate::setup::install_common::SetupOAuthClientId::Config { config } =
            &oauth.client_id
            && !config_paths.contains(config.as_str())
        {
            return Err(format!(
                "OAuth client id references unknown config: {config}"
            ));
        }
        if let Some(handle) = &oauth.client_secret_handle
            && !secret_names.contains(handle.as_str())
        {
            return Err(format!(
                "OAuth client secret handle is not declared by the package: {handle}"
            ));
        }
        if oauth.client_secret.is_some() && oauth.client_secret_handle.is_some() {
            return Err(
                "OAuth must not declare both a literal client secret and a secret handle"
                    .to_string(),
            );
        }
    }
    Ok(())
}

pub(super) fn account_identity(
    package: &ToolPackage,
    values: &BTreeMap<String, serde_json::Value>,
) -> Result<String, String> {
    let accounts = package
        .manifest
        .accounts
        .as_ref()
        .ok_or_else(|| "tool does not declare accounts".to_string())?;
    values
        .get(&accounts.identity_path)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|identity| !identity.is_empty() && !identity.contains('/'))
        .map(str::to_string)
        .ok_or_else(|| "account identity is missing or invalid".to_string())
}

pub(super) fn scoped_secret(account: &str, handle: &str) -> String {
    format!("{handle}:{account}").to_ascii_uppercase()
}

fn discover_tool_packages(layout: &RuntimeLayout) -> Vec<Result<ToolPackage, String>> {
    discover_package_dir_names(tool_source_dirs(layout))
        .into_iter()
        .map(|name| locate_tool_package(layout, &name.replace('-', "_")))
        .collect()
}

fn installed_tool_names(layout: &RuntimeLayout) -> Result<Vec<String>, String> {
    let mut names = crate::tools::wasm::discover(layout)
        .map_err(|err| err.to_string())?
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    Ok(names)
}

fn tool_source_dirs(layout: &RuntimeLayout) -> Vec<PathBuf> {
    vec![layout.root().join("tool-store")]
}

fn locate_wasm(source_dir: &Path, dir_name: &str) -> Result<PathBuf, String> {
    let flat = source_dir.join(format!("{dir_name}.wasm"));
    if flat.exists() {
        return Ok(flat);
    }
    let built = source_dir
        .join("target/wasm32-wasip2/release")
        .join(format!("{}.wasm", dir_name.replace('-', "_")));
    if built.exists() {
        return Ok(built);
    }
    Err(format!(
        "tool '{dir_name}' has no built WASM artifact in {}",
        source_dir.display()
    ))
}

pub(super) fn required_secrets(package: &ToolPackage) -> Vec<SetupSecret> {
    package.manifest.secrets.clone()
}

pub(super) fn required_config(package: &ToolPackage) -> Vec<SetupConfig> {
    package.manifest.config.clone()
}
