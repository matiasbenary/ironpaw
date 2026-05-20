use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde::Deserialize;

use crate::{config::RuntimeLayout, http::CredentialInjection};

#[derive(Clone, Debug)]
pub struct ChannelManifest {
    pub name: String,
    pub wasm_path: PathBuf,
    pub config_path: PathBuf,
    pub config_json: String,
    pub config: ChannelConfig,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChannelAuth {
    #[default]
    None,
    Bearer,
    HeaderSecret {
        header: String,
        value: Option<String>,
        secret: Option<String>,
    },
}

#[derive(Clone, Debug, Deserialize)]
pub struct ChannelConfig {
    pub path: String,
    #[serde(default)]
    pub auth: ChannelAuth,
    #[serde(default)]
    pub owner_id: Option<String>,
    #[serde(default)]
    pub http_credentials: Vec<ChannelHttpCredential>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ChannelHttpCredential {
    pub host: String,
    pub secret: String,
    #[serde(default)]
    pub placeholder: Option<String>,
    #[serde(default)]
    pub header: Option<String>,
    #[serde(default)]
    pub prefix: Option<String>,
}

impl ChannelHttpCredential {
    pub fn into_injection(self) -> CredentialInjection {
        if let Some(placeholder) = self.placeholder {
            return CredentialInjection::url_placeholder(self.host, placeholder, self.secret);
        }

        CredentialInjection {
            host: self.host,
            secret: self.secret,
            header: self.header.unwrap_or_else(|| "Authorization".to_string()),
            prefix: self.prefix.unwrap_or_else(|| "Bearer ".to_string()),
            placeholder: None,
        }
    }
}

pub fn discover(layout: &RuntimeLayout) -> io::Result<Vec<ChannelManifest>> {
    let mut manifests = Vec::new();
    let channels_dir = layout.channels_dir();

    if !channels_dir.exists() {
        return Ok(manifests);
    }

    for entry in fs::read_dir(&channels_dir)? {
        let entry = entry?;
        let wasm_path = entry.path();
        if wasm_path
            .extension()
            .and_then(|extension| extension.to_str())
            != Some("wasm")
        {
            continue;
        }

        let Some(name) = wasm_path
            .file_stem()
            .and_then(|file_stem| file_stem.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        let config_path = channel_config_path(&channels_dir, &name);
        if !config_path.exists() {
            continue;
        }

        let config_json = fs::read_to_string(&config_path)?;
        let config = serde_json::from_str(&config_json).map_err(io::Error::other)?;
        manifests.push(ChannelManifest {
            name,
            wasm_path,
            config_path,
            config_json,
            config,
        });
    }

    manifests.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(manifests)
}

pub fn inject_public_url(
    mut manifest: ChannelManifest,
    public_url: Option<&str>,
) -> io::Result<ChannelManifest> {
    let Some(public_url) = public_url else {
        return Ok(manifest);
    };

    let mut config_json = serde_json::from_str::<serde_json::Value>(&manifest.config_json)
        .map_err(io::Error::other)?;
    config_json["public_url"] = serde_json::Value::String(public_url.to_string());
    manifest.config_json = serde_json::to_string(&config_json).map_err(io::Error::other)?;
    Ok(manifest)
}

fn channel_config_path(channels_dir: &Path, name: &str) -> PathBuf {
    channels_dir.join(format!("{name}.config.json"))
}
