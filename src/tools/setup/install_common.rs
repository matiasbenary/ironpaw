use std::{collections::BTreeMap, path::Path};

#[derive(Clone, serde::Deserialize)]
pub struct SetupSecret {
    pub name: String,
    pub prompt: Option<String>,
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub auto_generate: Option<AutoGenerate>,
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
}

#[derive(Clone, Default, serde::Deserialize)]
pub enum ConfigValueType {
    #[serde(rename = "string")]
    #[default]
    String,
    #[serde(rename = "number")]
    Number,
    #[serde(rename = "bool", alias = "boolean")]
    Bool,
}

pub fn default_secret_length() -> usize {
    32
}

impl ConfigValueType {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConfigValueType::String => "string",
            ConfigValueType::Number => "number",
            ConfigValueType::Bool => "bool",
        }
    }
}

pub fn config_prompt(config: &SetupConfig) -> &str {
    config.prompt.as_deref().unwrap_or(&config.path)
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

pub fn env_secret_name(name: &str) -> String {
    if name.starts_with("WARDEN_") {
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
        if parts.peek().is_none() {
            let Some(object) = current.as_object_mut() else {
                return Err(format!(
                    "config path '/{path}' does not point into an object"
                ));
            };
            object.insert(part, replacement);
            return Ok(());
        }
        let Some(object) = current.as_object_mut() else {
            return Err(format!(
                "config path '/{path}' does not point into an object"
            ));
        };
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

pub fn write_env_secret(path: &Path, name: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let replacement = format!("{name}={}\n", quote_env_value(value));
    let mut found = false;
    let mut output = String::new();
    for line in existing.lines() {
        if line.trim_start().starts_with(&format!("{name}=")) {
            output.push_str(&replacement);
            found = true;
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }
    if !found {
        output.push_str(&replacement);
    }
    std::fs::write(path, output).map_err(|err| err.to_string())
}

pub fn quote_env_value(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

pub fn reload_env(path: &Path) -> Result<(), String> {
    dotenvy::from_path_override(path).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_generated_secret_is_hex_sized() {
        let secret = SetupSecret {
            name: "x".to_string(),
            prompt: Some("x".to_string()),
            optional: true,
            auto_generate: Some(AutoGenerate { length: 8 }),
        };

        let value = secret_value(&secret, "").unwrap();

        assert_eq!(value.len(), 16);
        assert!(value.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn applies_install_config_values_to_template() {
        let mut config = serde_json::json!({
            "owner_id": null,
            "telegram": {
                "webhook_enabled": true
            }
        });
        let values = BTreeMap::from([(
            "/owner_id".to_string(),
            serde_json::Value::String("123456789".to_string()),
        )]);

        apply_config_values(&mut config, &values).unwrap();

        assert_eq!(config["owner_id"], "123456789");
        assert_eq!(config["telegram"]["webhook_enabled"], true);
    }
}
