use std::{env, path::PathBuf};

use crate::config::RuntimeLayout;

#[derive(Clone, Debug)]
pub struct Secrets {
    loaded_env: Option<PathBuf>,
    pub channels: ChannelSecrets,
    pub tools: ToolSecrets,
}

impl Secrets {
    pub fn load(layout: &RuntimeLayout) -> Self {
        Self {
            loaded_env: load_env(layout),
            channels: ChannelSecrets::new(),
            tools: ToolSecrets::new(),
        }
    }

    pub fn loaded_env(&self) -> Option<&PathBuf> {
        self.loaded_env.as_ref()
    }
}

#[derive(Clone, Debug, Default)]
pub struct ChannelSecrets;

impl ChannelSecrets {
    fn new() -> Self {
        Self
    }

    pub fn get(&self, name: &str) -> Option<String> {
        env::var(name).ok()
    }
}

impl crate::http::SecretLookup for ChannelSecrets {
    fn get_secret(&self, name: &str) -> Option<String> {
        self.get(name)
    }
}

#[derive(Clone, Debug, Default)]
pub struct ToolSecrets;

impl ToolSecrets {
    fn new() -> Self {
        Self
    }

    pub fn get(&self, name: &str) -> Option<String> {
        env::var(name).ok()
    }
}

impl crate::http::SecretLookup for ToolSecrets {
    fn get_secret(&self, name: &str) -> Option<String> {
        self.get(name)
    }
}

fn load_env(layout: &RuntimeLayout) -> Option<PathBuf> {
    let candidates = [
        env::var_os("WARDEN_ENV").map(PathBuf::from),
        Some(layout.root().join(".env")),
        env::current_dir().ok().map(|path| path.join(".env")),
    ];

    candidates
        .into_iter()
        .flatten()
        .find(|candidate| dotenvy::from_path(candidate).is_ok())
}
