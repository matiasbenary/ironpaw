use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use crate::{
    config::RuntimeLayout,
    shell_tools::ShellToolManifest,
    tools::{
        setup::{
            install_common::{
                SetupConfig, SetupSecret, config_statuses, discover_package_dir_names,
                locate_config_template, secret_statuses, write_package_artifacts,
            },
            install_tool::ListToolsTool,
        },
        wasm::{WasmToolConfig, WasmToolManifest},
    },
};

use super::installable_tool::discover_shell_packages;

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
    let config = serde_json::from_str(&config_json).map_err(|err| err.to_string())?;
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
            let installed = installed.contains(&package.name);
            package_names.insert(package.name.clone());
            tool_rows.push(serde_json::json!({
                "name": package.name,
                "kind": "wasm",
                "display_name": package.display_name,
                "description": package.manifest.description,
                "installed": installed,
                "installable": !installed,
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
            if installed_shell_names.contains(package.manifest.name.as_str()) {
                continue;
            }
            tool_rows.push(shell_inventory_row(
                &package.manifest,
                false,
                false,
                &self.secrets,
            ));
        }
        for manifest in installed_shell {
            let is_enabled = enabled.is_enabled(&manifest.name)?;
            tool_rows.push(shell_inventory_row(
                &manifest,
                true,
                is_enabled,
                &self.secrets,
            ));
        }

        tool_rows.sort_by(|left, right| {
            left["name"]
                .as_str()
                .unwrap_or_default()
                .cmp(right["name"].as_str().unwrap_or_default())
        });

        Ok(serde_json::json!({
            "ok": true,
            "tools": tool_rows,
        }))
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
        "commands": manifest.commands,
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
    let crate_name = dir_name.replace('-', "_");
    let built = source_dir
        .join("target")
        .join("wasm32-wasip2")
        .join("release")
        .join(format!("{crate_name}.wasm"));
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

pub(super) fn install_package(
    layout: &RuntimeLayout,
    package: &ToolPackage,
    config_values: &BTreeMap<String, serde_json::Value>,
) -> Result<WasmToolManifest, String> {
    let (wasm_dst, config_dst, config_json) = write_package_artifacts(
        "tool",
        &layout.tools_dir(),
        &package.name,
        &package.wasm_path,
        &package.config_template_path,
        config_values,
        true,
    )?;
    let parsed: WasmToolConfig =
        serde_json::from_str(&config_json).map_err(|err| err.to_string())?;
    Ok(WasmToolManifest {
        name: package.name.clone(),
        wasm_path: wasm_dst,
        config_path: config_dst,
        config_json,
        config: parsed,
    })
}

pub(super) fn reconfigure_package(
    layout: &RuntimeLayout,
    package: &ToolPackage,
    config_values: &BTreeMap<String, serde_json::Value>,
) -> Result<WasmToolManifest, String> {
    let Some(mut manifest) = installed_tool_manifest(layout, &package.name)? else {
        return Err(format!("tool '{}' is not installed", package.name));
    };
    let mut config: serde_json::Value =
        serde_json::from_str(&manifest.config_json).map_err(|err| err.to_string())?;
    crate::tools::setup::install_common::apply_config_values(&mut config, config_values)?;
    manifest.config_json = serde_json::to_string_pretty(&config).map_err(|err| err.to_string())?;
    std::fs::write(&manifest.config_path, &manifest.config_json).map_err(|err| err.to_string())?;
    manifest.config = serde_json::from_value(config).map_err(|err| err.to_string())?;
    Ok(manifest)
}
