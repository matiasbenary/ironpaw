use std::path::{Path, PathBuf};

use crate::{
    config::RuntimeLayout,
    shell_tools::{MANIFEST_FILENAME, ShellToolManifest},
};

use crate::setup::{
    common::{SetupConfig, SetupSecret, stored_secret_name},
    tool::package::{
        SetupAccounts, ToolPackage, locate_tool_package, required_config, required_secrets,
    },
};

#[derive(Clone)]
pub(super) enum InstallableTool {
    Wasm(ToolPackage),
    Shell(ShellPackage),
}

#[derive(Clone)]
pub(super) struct ShellPackage {
    pub(super) source_dir: PathBuf,
    pub(super) manifest: ShellToolManifest,
}

impl InstallableTool {
    pub(super) fn locate(layout: &RuntimeLayout, name: &str) -> Result<Self, String> {
        if let Some(package) = locate_shell_package(layout, name)? {
            return Ok(Self::Shell(package));
        }
        locate_tool_package(layout, name).map(Self::Wasm)
    }

    pub(super) fn name(&self) -> &str {
        match self {
            Self::Wasm(package) => &package.name,
            Self::Shell(package) => &package.manifest.name,
        }
    }

    pub(super) fn display_name(&self) -> &str {
        match self {
            Self::Wasm(package) => &package.display_name,
            Self::Shell(package) => &package.manifest.name,
        }
    }

    pub(super) fn setup_url(&self) -> Option<&str> {
        match self {
            Self::Wasm(package) => package.manifest.setup_url.as_deref(),
            Self::Shell(_) => None,
        }
    }

    pub(super) fn required_secrets(&self) -> Vec<SetupSecret> {
        match self {
            Self::Wasm(package) => required_secrets(package),
            Self::Shell(package) => package
                .manifest
                .secrets
                .iter()
                .map(|secret| SetupSecret {
                    name: secret.name.clone(),
                    path: None,
                    prompt: Some(format!(
                        "Value for {} (injected as {})",
                        secret.name, secret.env
                    )),
                    optional: false,
                    auto_generate: None,
                    when: None,
                })
                .collect(),
        }
    }

    pub(super) fn required_config(&self) -> Vec<SetupConfig> {
        match self {
            Self::Wasm(package) => required_config(package),
            Self::Shell(_) => Vec::new(),
        }
    }

    pub(super) fn config_before_secrets(&self) -> bool {
        matches!(self, Self::Wasm(package) if package.manifest.config_before_secrets)
    }

    pub(super) fn setup_handler(&self) -> Option<&str> {
        match self {
            Self::Wasm(package) => package.manifest.setup_handler.as_deref(),
            Self::Shell(_) => None,
        }
    }

    pub(super) fn accounts(&self) -> Option<&SetupAccounts> {
        match self {
            Self::Wasm(package) => package.manifest.accounts.as_ref(),
            Self::Shell(_) => None,
        }
    }

    pub(super) fn secret_store_name(&self, name: &str) -> String {
        match self {
            Self::Wasm(_) => stored_secret_name(name),
            Self::Shell(package) => package.manifest.secret_store_name(name),
        }
    }

    pub(super) fn approval_prompt(&self) -> String {
        match self {
            Self::Wasm(package) => {
                let capabilities = wasm_capabilities(package);
                format!("Install the {} tool? {capabilities}", self.name())
            }
            Self::Shell(package) => {
                let commands = package
                    .manifest
                    .commands
                    .iter()
                    .map(|command| command.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let hosts = if package.manifest.allowed_hosts.is_empty() {
                    "none".to_string()
                } else {
                    package.manifest.allowed_hosts.join(", ")
                };
                let secrets = if package.manifest.secrets.is_empty() {
                    "none".to_string()
                } else {
                    package
                        .manifest
                        .secrets
                        .iter()
                        .map(|secret| format!("{} -> {}", secret.name, secret.env))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                format!(
                    "Install the {} shell tool? Commands: {commands}. Network hosts: {hosts}. Secrets: {secrets}. Installer: {:?}.",
                    package.manifest.name, package.manifest.install
                )
            }
        }
    }
}

fn wasm_capabilities(package: &ToolPackage) -> String {
    let config: serde_json::Value = std::fs::read_to_string(&package.config_template_path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let mut hosts = config["http_credentials"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|grant| grant["host"].as_str())
        .collect::<Vec<_>>();
    hosts.extend(
        package
            .manifest
            .config
            .iter()
            .flat_map(|config| &config.choices)
            .filter_map(|choice| choice.oauth.as_ref())
            .map(|oauth| oauth.host.as_str()),
    );
    hosts.sort_unstable();
    hosts.dedup();
    let oauth_endpoints = package
        .manifest
        .config
        .iter()
        .flat_map(|config| &config.choices)
        .filter_map(|choice| choice.oauth.as_ref())
        .flat_map(|oauth| [oauth.authorize_url.as_ref(), oauth.token_url.as_ref()])
        .flatten()
        .collect::<std::collections::BTreeSet<_>>();
    let scopes = package
        .manifest
        .config
        .iter()
        .flat_map(|config| &config.choices)
        .filter_map(|choice| choice.oauth.as_ref())
        .flat_map(|oauth| &oauth.scopes)
        .collect::<std::collections::BTreeSet<_>>();
    format!(
        "Network hosts: {}. OAuth endpoints: {}. OAuth scopes: {}.",
        if hosts.is_empty() {
            "none".to_string()
        } else {
            hosts.join(", ")
        },
        if oauth_endpoints.is_empty() {
            "none".to_string()
        } else {
            oauth_endpoints
                .into_iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        },
        if scopes.is_empty() {
            "none".to_string()
        } else {
            scopes.into_iter().cloned().collect::<Vec<_>>().join(", ")
        }
    )
}

pub(super) fn discover_shell_packages(layout: &RuntimeLayout) -> Vec<Result<ShellPackage, String>> {
    let root = layout.root().join("tool-store");
    std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join(MANIFEST_FILENAME).is_file())
        .map(|entry| read_shell_package(&entry.path()))
        .collect()
}

fn locate_shell_package(
    layout: &RuntimeLayout,
    name: &str,
) -> Result<Option<ShellPackage>, String> {
    let root = layout.root().join("tool-store");
    for directory in [root.join(name), root.join(name.replace('_', "-"))] {
        if directory.join(MANIFEST_FILENAME).is_file() {
            let package = read_shell_package(&directory)?;
            if package.manifest.name != name {
                return Err(format!(
                    "shell manifest declares tool '{}' but package is '{name}'",
                    package.manifest.name
                ));
            }
            return Ok(Some(package));
        }
    }
    Ok(None)
}

fn read_shell_package(source_dir: &Path) -> Result<ShellPackage, String> {
    let path = source_dir.join(MANIFEST_FILENAME);
    let manifest: ShellToolManifest = serde_json::from_str(
        &std::fs::read_to_string(&path)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?,
    )
    .map_err(|error| format!("invalid {}: {error}", path.display()))?;
    manifest.validate()?;
    if let Some(skill) = &manifest.skill
        && !source_dir.join(skill).is_file()
    {
        return Err(format!("shell tool '{}' is missing {skill}", manifest.name));
    }
    Ok(ShellPackage {
        source_dir: source_dir.to_path_buf(),
        manifest,
    })
}
