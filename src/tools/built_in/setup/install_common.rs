use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use crate::secrets::Secrets;

#[derive(Clone, serde::Deserialize)]
pub struct SetupSecret {
    pub name: String,
    pub prompt: Option<String>,
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub auto_generate: Option<AutoGenerate>,
    #[serde(default)]
    pub when: Option<SetupCondition>,
}

#[derive(Clone, serde::Deserialize)]
pub struct AutoGenerate {
    #[serde(default = "default_secret_length")]
    pub length: usize,
}

#[derive(Clone, serde::Deserialize)]
pub struct SetupConfig {
    pub path: String,
    pub prompt: Option<String>,
    #[serde(default, rename = "type")]
    pub value_type: ConfigValueType,
    #[serde(default)]
    pub autoconfigure: Option<SetupAutoconfigure>,
    #[serde(default)]
    pub when: Option<SetupCondition>,
    #[serde(default)]
    pub choices: Vec<SetupChoice>,
}

#[derive(Clone, serde::Deserialize)]
pub struct SetupCondition {
    pub path: String,
    pub equals: serde_json::Value,
}

#[derive(Clone, serde::Deserialize)]
pub struct SetupChoice {
    pub label: String,
    pub value: String,
    #[serde(default)]
    pub oauth: Option<SetupOAuth>,
}

#[derive(Clone, serde::Deserialize)]
pub struct SetupOAuth {
    pub login_hint_path: String,
    pub authorize_url: String,
    pub token_url: String,
    pub client_id: SetupOAuthClientId,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub client_secret_handle: Option<String>,
    pub redirect_host: String,
    pub refresh_token_handle: String,
    pub scopes: Vec<String>,
    #[serde(default)]
    pub extra_authorize_params: BTreeMap<String, String>,
}

#[derive(Clone, serde::Deserialize)]
#[serde(untagged)]
pub enum SetupOAuthClientId {
    Literal(String),
    Config { config: String },
}

impl SetupOAuthClientId {
    pub fn resolve(&self, values: &BTreeMap<String, serde_json::Value>) -> Result<String, String> {
        match self {
            Self::Literal(value) if !value.trim().is_empty() => Ok(value.clone()),
            Self::Literal(_) => Err("OAuth client id must not be empty".to_string()),
            Self::Config { config } => values
                .get(config)
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
                .ok_or_else(|| format!("OAuth client id config is missing: {config}")),
        }
    }
}

#[derive(Clone, serde::Deserialize)]
pub struct SetupAutoconfigure {
    pub action: String,
    #[serde(default)]
    pub rerun_on_keep: bool,
}

#[derive(Clone, Copy, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigValueType {
    #[default]
    String,
    Email,
    Number,
    #[serde(alias = "boolean")]
    Bool,
}

pub fn default_secret_length() -> usize {
    32
}

pub fn config_prompt(config: &SetupConfig) -> &str {
    config.prompt.as_deref().unwrap_or(&config.path)
}

pub fn config_statuses(configs: Vec<SetupConfig>) -> Vec<serde_json::Value> {
    configs
        .into_iter()
        .map(|config| {
            serde_json::json!({
                "path": config.path,
                "type": config.value_type,
                "prompt": config_prompt(&config),
            })
        })
        .collect()
}

pub fn secret_statuses(
    secrets: Vec<SetupSecret>,
    secrets_store: &Secrets,
    include_auto_generate: bool,
) -> Vec<serde_json::Value> {
    secrets
        .into_iter()
        .map(|secret| {
            let secret_name = stored_secret_name(&secret.name);
            let mut status = serde_json::json!({
                "name": secret.name,
                "secret": secret_name,
                "configured": secrets_store.exists(&secret_name),
                "optional": secret.optional,
            });
            if include_auto_generate {
                status["auto_generate"] = serde_json::Value::Bool(secret.auto_generate.is_some());
            }
            status
        })
        .collect()
}

pub fn secret_value(secret: &SetupSecret, message: &str) -> Result<String, String> {
    let value = message.trim();
    if value.is_empty() {
        if let Some(auto) = &secret.auto_generate {
            return Ok(generate_secret(auto.length));
        }
        if secret.optional {
            return Ok(String::new());
        }
        return Err("secret value must not be empty".to_string());
    }
    Ok(value.to_string())
}

pub fn config_value(config: &SetupConfig, message: &str) -> Result<serde_json::Value, String> {
    let value = message.trim();
    if value.is_empty() {
        return Err("config value must not be empty".to_string());
    }
    match config.value_type {
        ConfigValueType::String => Ok(serde_json::Value::String(value.to_string())),
        ConfigValueType::Email => {
            if !crate::network::email::is_email_address(value) {
                return Err(format!("{} must be a valid email address", config.path));
            }
            Ok(serde_json::Value::String(value.to_string()))
        }
        ConfigValueType::Number => {
            let number = value
                .parse::<i64>()
                .map_err(|_| format!("{} must be a number", config.path))?;
            Ok(serde_json::Value::Number(number.into()))
        }
        ConfigValueType::Bool => match value.to_ascii_lowercase().as_str() {
            "true" | "yes" | "y" => Ok(serde_json::Value::Bool(true)),
            "false" | "no" | "n" => Ok(serde_json::Value::Bool(false)),
            _ => Err(format!("{} must be true or false", config.path)),
        },
    }
}

pub fn stored_secret_name(name: &str) -> String {
    if name.starts_with("IRONPAW_") {
        name.to_string()
    } else {
        name.to_ascii_uppercase()
    }
}

pub fn generate_secret(length: usize) -> String {
    let mut output = String::new();
    while output.len() < length * 2 {
        output.push_str(&uuid::Uuid::new_v4().simple().to_string());
    }
    output.truncate(length * 2);
    output
}

pub fn apply_config_values(
    config: &mut serde_json::Value,
    values: &BTreeMap<String, serde_json::Value>,
) -> Result<(), String> {
    for (path, value) in values {
        set_json_pointer(config, path, value.clone())?;
    }
    Ok(())
}

pub fn locate_config_template(
    source_dir: &Path,
    name: &str,
    kind: &str,
) -> Result<PathBuf, String> {
    for file_name in [
        format!("{name}.config.json"),
        format!("{name}.config.template.json"),
    ] {
        let path = source_dir.join(file_name);
        if path.exists() {
            return Ok(path);
        }
    }
    Err(format!(
        "{kind} '{name}' has no config template in {}",
        source_dir.display()
    ))
}

pub fn write_package_artifacts(
    kind: &str,
    install_dir: &Path,
    name: &str,
    wasm_path: &Path,
    config_template_path: &Path,
    config_values: &BTreeMap<String, serde_json::Value>,
    reject_existing: bool,
) -> Result<(PathBuf, PathBuf, String), String> {
    std::fs::create_dir_all(install_dir).map_err(|err| err.to_string())?;
    let wasm_dst = install_dir.join(format!("{name}.wasm"));
    let config_dst = install_dir.join(format!("{name}.config.json"));
    if reject_existing && (wasm_dst.exists() || config_dst.exists()) {
        return Err(format!("{kind} '{name}' is already installed"));
    }
    let mut config: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(config_template_path)
            .map_err(|err| format!("could not read {}: {err}", config_template_path.display()))?,
    )
    .map_err(|err| format!("could not parse {}: {err}", config_template_path.display()))?;
    apply_config_values(&mut config, config_values)?;

    std::fs::copy(wasm_path, &wasm_dst).map_err(|err| err.to_string())?;
    let config_json = serde_json::to_string_pretty(&config).map_err(|err| err.to_string())?;
    std::fs::write(&config_dst, &config_json).map_err(|err| err.to_string())?;
    Ok((wasm_dst, config_dst, config_json))
}

pub fn discover_package_dir_names(source_dirs: Vec<PathBuf>) -> Vec<String> {
    source_dirs
        .into_iter()
        .flat_map(|root| std::fs::read_dir(root).into_iter().flatten())
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            entry
                .path()
                .join(format!("{name}.install.json"))
                .exists()
                .then_some(name)
        })
        .collect()
}

pub fn set_json_pointer(
    value: &mut serde_json::Value,
    path: &str,
    replacement: serde_json::Value,
) -> Result<(), String> {
    let Some(path) = path.strip_prefix('/') else {
        return Err(format!("config path '{path}' must start with '/'"));
    };
    let mut current = value;
    let mut parts = path.split('/').map(pointer_part).peekable();
    while let Some(part) = parts.next() {
        let object = current
            .as_object_mut()
            .ok_or_else(|| format!("config path '/{path}' does not point into an object"))?;
        if parts.peek().is_none() {
            object.insert(part, replacement);
            return Ok(());
        }
        current = object
            .entry(part)
            .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    }
    Err("config path must not be empty".to_string())
}

fn pointer_part(part: &str) -> String {
    part.replace("~1", "/").replace("~0", "~")
}

pub fn is_yes(message: &str) -> bool {
    matches!(
        message.trim().to_ascii_lowercase().as_str(),
        "y" | "yes" | "ok" | "okay" | "sure" | "confirm" | "install"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_client_id_can_be_publisher_or_operator_provided() {
        let values = BTreeMap::from([(
            "/oauth/client_id".to_string(),
            serde_json::json!("operator-client"),
        )]);

        assert_eq!(
            SetupOAuthClientId::Literal("publisher-client".to_string())
                .resolve(&values)
                .unwrap(),
            "publisher-client"
        );
        assert_eq!(
            SetupOAuthClientId::Config {
                config: "/oauth/client_id".to_string()
            }
            .resolve(&values)
            .unwrap(),
            "operator-client"
        );
    }
}
