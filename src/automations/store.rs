use std::{fs, path::PathBuf};

use super::Automation;

/// Persisted automation definitions. The host, not the agent file tools,
/// owns their serialization.
pub struct FileAutomations {
    dir: PathBuf,
}

impl FileAutomations {
    pub fn new(dir: PathBuf) -> Result<Self, String> {
        fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
        Ok(Self { dir })
    }

    pub fn write(&self, automation: &Automation) -> Result<(), String> {
        fs::write(
            self.path(&automation.name),
            format!(
                "{}\n",
                serde_json::to_string_pretty(automation).map_err(|err| err.to_string())?
            ),
        )
        .map_err(|err| err.to_string())
    }

    pub fn delete(&self, name: &str) -> Result<(), String> {
        fs::remove_file(self.path(name)).map_err(|err| err.to_string())
    }

    pub fn load_all(&self) -> Result<Vec<Automation>, String> {
        let mut automations = fs::read_dir(&self.dir)
            .map_err(|err| err.to_string())?
            .map(|entry| entry.map_err(|err| err.to_string()))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("json"))
            .map(|entry| {
                let path = entry.path();
                serde_json::from_str::<Automation>(
                    &fs::read_to_string(&path).map_err(|err| err.to_string())?,
                )
                .map_err(|err| format!("invalid {}: {err}", path.display()))
            })
            .collect::<Result<Vec<_>, String>>()?;
        automations.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(automations)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.json"))
    }
}
