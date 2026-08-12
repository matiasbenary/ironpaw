use std::{collections::BTreeSet, fs, path::PathBuf};

#[derive(Clone)]
pub struct EnabledTools {
    path: PathBuf,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct StoredEnabledTools {
    #[serde(default)]
    enabled: BTreeSet<String>,
}

impl Default for StoredEnabledTools {
    fn default() -> Self {
        Self {
            enabled: ["web_search".to_string()].into_iter().collect(),
        }
    }
}

impl EnabledTools {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn is_enabled(&self, name: &str) -> Result<bool, String> {
        Ok(self.load()?.enabled.contains(name))
    }

    pub fn enable(&self, name: &str) -> Result<(), String> {
        let mut state = self.load()?;
        state.enabled.insert(name.to_string());
        self.save(&state)
    }

    fn load(&self) -> Result<StoredEnabledTools, String> {
        if !self.path.exists() {
            return Ok(StoredEnabledTools::default());
        }
        let mut state: StoredEnabledTools =
            serde_json::from_str(&fs::read_to_string(&self.path).map_err(|err| err.to_string())?)
                .map_err(|err| format!("invalid {}: {err}", self.path.display()))?;
        state.enabled.insert("web_search".to_string());
        Ok(state)
    }

    fn save(&self, state: &StoredEnabledTools) -> Result<(), String> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| format!("tools state path has no parent: {}", self.path.display()))?;
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        fs::write(
            &self.path,
            format!(
                "{}\n",
                serde_json::to_string_pretty(state).map_err(|err| err.to_string())?
            ),
        )
        .map_err(|err| err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabled_tool_persists() {
        let path =
            std::env::temp_dir().join(format!("ironpaw-tools-{}.json", uuid::Uuid::new_v4()));
        let state = EnabledTools::new(path.clone());
        assert!(state.is_enabled("web_search").unwrap());
        assert!(!state.is_enabled("email").unwrap());
        state.enable("email").unwrap();
        assert!(EnabledTools::new(path.clone()).is_enabled("email").unwrap());
        fs::remove_file(path).unwrap();
    }
}
