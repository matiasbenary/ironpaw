use std::{
    collections::HashSet,
    path::{Component, Path},
};

pub const MANIFEST_FILENAME: &str = "shell.json";
pub const DEFAULT_COMMANDS: [&str; 9] =
    ["cat", "find", "head", "jq", "ls", "rg", "sed", "tail", "wc"];

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShellToolManifest {
    pub name: String,
    pub description: String,
    pub install: InstallMethod,
    pub commands: Vec<ShellCommand>,
    #[serde(default)]
    pub skill: Option<String>,
    #[serde(default)]
    pub secrets: Vec<ShellSecret>,
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "kind")]
pub enum InstallMethod {
    Preinstalled {
        name: String,
    },
    Wget {
        url: String,
        #[serde(default)]
        run_with_sh: bool,
    },
    Curl {
        url: String,
        #[serde(default)]
        run_with_sh: bool,
    },
    Npm {
        package: String,
    },
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShellCommand {
    pub name: String,
    pub invocations: Vec<ShellInvocation>,
    #[serde(default)]
    pub environment: std::collections::BTreeMap<String, String>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShellInvocation {
    #[serde(default)]
    pub args_prefix: Vec<String>,
    #[serde(default)]
    pub secrets: Vec<String>,
    #[serde(default)]
    pub options: Vec<ShellOption>,
    #[serde(default)]
    pub min_positionals: usize,
    #[serde(default)]
    pub max_positionals: Option<usize>,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShellOption {
    pub name: String,
    #[serde(default)]
    pub takes_value: bool,
    #[serde(default)]
    pub repeatable: bool,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShellSecret {
    pub name: String,
    pub env: String,
}

impl ShellToolManifest {
    pub fn secret_store_name(&self, secret_name: &str) -> String {
        format!("shell/{}/{}", self.name, secret_name)
    }

    pub fn discover(tools_dir: &Path) -> Result<Vec<Self>, String> {
        if !tools_dir.exists() {
            return Ok(Vec::new());
        }
        let mut manifests = Vec::new();
        for entry in std::fs::read_dir(tools_dir).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            let tool_dir = entry.path();
            if tool_dir.is_dir() && tool_dir.join(MANIFEST_FILENAME).is_file() {
                manifests.push(Self::load(&tool_dir)?);
            }
        }
        manifests.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(manifests)
    }

    pub fn load_named(tools_dir: &Path, name: &str) -> Result<Self, String> {
        validate_name(name, "shell tool")?;
        Self::load(&tools_dir.join(name))
    }

    pub fn load(tool_dir: &Path) -> Result<Self, String> {
        let path = tool_dir.join(MANIFEST_FILENAME);
        let contents = std::fs::read_to_string(&path)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
        let manifest: Self = serde_json::from_str(&contents)
            .map_err(|error| format!("invalid {}: {error}", path.display()))?;
        manifest.validate()?;
        let directory_name = tool_dir
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("invalid shell tool directory: {}", tool_dir.display()))?;
        if manifest.name != directory_name {
            return Err(format!(
                "shell manifest name '{}' does not match directory '{directory_name}'",
                manifest.name
            ));
        }
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<(), String> {
        validate_name(&self.name, "shell tool")?;
        require_text(&self.description, "shell tool description")?;
        validate_relative_file(self.skill.as_deref(), "skill")?;
        validate_unique(self.allowed_hosts.iter(), "allowed host")?;
        for host in &self.allowed_hosts {
            validate_host(host)?;
        }
        self.install.validate(&self.allowed_hosts)?;
        validate_unique(
            self.secrets.iter().map(|secret| &secret.name),
            "secret name",
        )?;
        validate_unique(self.secrets.iter().map(|secret| &secret.env), "secret env")?;
        for secret in &self.secrets {
            require_text(&secret.name, "secret name")?;
            validate_env_name(&secret.env)?;
        }
        if self.commands.is_empty() {
            return Err("shell tool must declare at least one command".to_string());
        }
        validate_unique(self.commands.iter().map(|command| &command.name), "command")?;
        for command in &self.commands {
            command.validate(&self.secrets)?;
        }
        Ok(())
    }
}

impl InstallMethod {
    fn validate(&self, allowed_hosts: &[String]) -> Result<(), String> {
        match self {
            Self::Preinstalled { name } => validate_name(name, "preinstalled command"),
            Self::Wget { url, .. } | Self::Curl { url, .. } => {
                let url = reqwest::Url::parse(url)
                    .map_err(|error| format!("invalid installer URL: {error}"))?;
                if url.scheme() != "https" {
                    return Err("installer URLs must use https".to_string());
                }
                let host = url
                    .host_str()
                    .ok_or_else(|| "installer URL must have a host".to_string())?;
                if !allowed_hosts.iter().any(|allowed| allowed == host) {
                    return Err(format!("installer host '{host}' is not in allowed_hosts"));
                }
                Ok(())
            }
            Self::Npm { package } => {
                if package.is_empty()
                    || package.starts_with('-')
                    || package.contains('\0')
                    || package.chars().any(char::is_whitespace)
                {
                    return Err("invalid npm package name".to_string());
                }
                if !allowed_hosts
                    .iter()
                    .any(|host| host == "registry.npmjs.org")
                {
                    return Err("npm installers must allow host 'registry.npmjs.org'".to_string());
                }
                Ok(())
            }
        }
    }
}

impl ShellCommand {
    fn validate(&self, declared_secrets: &[ShellSecret]) -> Result<(), String> {
        validate_name(&self.name, "command")?;
        for (name, value) in &self.environment {
            validate_env_name(name)?;
            if value.contains('\0') {
                return Err(format!(
                    "command environment variable '{name}' contains a null byte"
                ));
            }
        }
        if self.invocations.is_empty() {
            return Err(format!(
                "command '{}' must allow at least one invocation",
                self.name
            ));
        }
        for invocation in &self.invocations {
            invocation.validate(&self.name, declared_secrets)?;
        }
        Ok(())
    }
}

impl ShellInvocation {
    fn validate(&self, command: &str, declared_secrets: &[ShellSecret]) -> Result<(), String> {
        if self
            .max_positionals
            .is_some_and(|maximum| maximum < self.min_positionals)
        {
            return Err(format!(
                "command '{command}' has max_positionals below min_positionals"
            ));
        }
        for arg in &self.args_prefix {
            require_text(arg, "argument prefix")?;
            if arg.starts_with('-') {
                return Err(format!(
                    "command '{command}' argument prefixes cannot be options"
                ));
            }
        }
        validate_unique(self.secrets.iter(), "invocation secret")?;
        for secret in &self.secrets {
            if !declared_secrets
                .iter()
                .any(|declared| declared.name == *secret)
            {
                return Err(format!(
                    "command '{command}' references undeclared secret '{secret}'"
                ));
            }
        }
        validate_unique(self.options.iter().map(|option| &option.name), "option")?;
        for option in &self.options {
            if !option.name.starts_with('-')
                || option.name == "-"
                || option.name.contains(['=', '\0'])
                || option.name.chars().any(char::is_whitespace)
            {
                return Err(format!("invalid command option: {}", option.name));
            }
        }
        Ok(())
    }
}

fn validate_name(value: &str, kind: &str) -> Result<(), String> {
    if value.is_empty()
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        return Err(format!(
            "{kind} name must use only ASCII letters, digits, '_' or '-'"
        ));
    }
    Ok(())
}

fn require_text(value: &str, kind: &str) -> Result<(), String> {
    if value.trim().is_empty() || value.contains('\0') {
        return Err(format!("{kind} must not be empty"));
    }
    Ok(())
}

fn validate_env_name(name: &str) -> Result<(), String> {
    let mut characters = name.chars();
    if !characters
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
        || !characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
    {
        return Err(format!("invalid secret environment variable: {name}"));
    }
    Ok(())
}

fn validate_host(host: &str) -> Result<(), String> {
    if host.is_empty()
        || host != host.to_ascii_lowercase()
        || host.contains(['/', ':', '@', '*'])
        || host.starts_with('.')
        || host.ends_with('.')
        || host.split('.').any(|label| {
            label.is_empty()
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-')
        })
    {
        return Err(format!("invalid allowed host: {host}"));
    }
    Ok(())
}

fn validate_relative_file(path: Option<&str>, kind: &str) -> Result<(), String> {
    if let Some(path) = path {
        validate_relative_path(path, kind)?;
    }
    Ok(())
}

fn validate_relative_path(path: &str, kind: &str) -> Result<(), String> {
    if path.is_empty()
        || Path::new(path)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(format!("{kind} must be a safe relative path"));
    }
    Ok(())
}

fn validate_unique<T, I>(values: I, kind: &str) -> Result<(), String>
where
    T: AsRef<str>,
    I: IntoIterator<Item = T>,
{
    let mut seen = HashSet::new();
    for value in values {
        if !seen.insert(value.as_ref().to_string()) {
            return Err(format!("duplicate {kind}: {}", value.as_ref()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_manifest() -> ShellToolManifest {
        serde_json::from_value(serde_json::json!({
            "name": "github",
            "description": "Work with GitHub repositories",
            "install": {
                "kind": "curl",
                "url": "https://github.com/cli/cli/releases/download/example/install.sh",
                "run_with_sh": true
            },
            "skill": "SKILL.md",
            "secrets": [{ "name": "GITHUB_TOKEN", "env": "GH_TOKEN" }],
            "allowed_hosts": ["api.github.com", "github.com"],
            "commands": [
                {
                    "name": "gh",
                    "invocations": [{
                        "args_prefix": ["issue", "list"],
                        "secrets": ["GITHUB_TOKEN"],
                        "options": [
                            { "name": "--repo", "takes_value": true },
                            { "name": "--json", "takes_value": true }
                        ]
                    }]
                },
                {
                    "name": "git",
                    "environment": { "GIT_TERMINAL_PROMPT": "0" },
                    "invocations": [{
                        "args_prefix": ["status"]
                    }]
                }
            ]
        }))
        .unwrap()
    }

    #[test]
    fn rejects_unsafe_install_method() {
        let mut manifest = valid_manifest();
        manifest.install = InstallMethod::Curl {
            url: "http://github.com/install.sh".to_string(),
            run_with_sh: true,
        };
        assert_eq!(
            manifest.validate().unwrap_err(),
            "installer URLs must use https"
        );

        let mut manifest = valid_manifest();
        manifest.install = InstallMethod::Wget {
            url: "https://evil.example/install.sh".to_string(),
            run_with_sh: true,
        };
        assert_eq!(
            manifest.validate().unwrap_err(),
            "installer host 'evil.example' is not in allowed_hosts"
        );
    }

    #[test]
    fn secret_store_names_are_package_local() {
        let mut github = valid_manifest();
        github.name = "github".to_string();
        let mut other = valid_manifest();
        other.name = "other".to_string();

        assert_eq!(
            github.secret_store_name("GITHUB_TOKEN"),
            "shell/github/GITHUB_TOKEN"
        );
        assert_ne!(
            github.secret_store_name("GITHUB_TOKEN"),
            other.secret_store_name("GITHUB_TOKEN")
        );
    }

    #[test]
    fn rejects_unsafe_capabilities() {
        let mut manifest = valid_manifest();
        manifest.allowed_hosts = vec!["https://github.com".to_string()];
        assert_eq!(
            manifest.validate().unwrap_err(),
            "invalid allowed host: https://github.com"
        );

        let mut manifest = valid_manifest();
        manifest.secrets[0].env = "GH-TOKEN".to_string();
        assert_eq!(
            manifest.validate().unwrap_err(),
            "invalid secret environment variable: GH-TOKEN"
        );

        let mut manifest = valid_manifest();
        manifest.commands[0].invocations[0].secrets = vec!["OTHER_TOKEN".to_string()];
        assert_eq!(
            manifest.validate().unwrap_err(),
            "command 'gh' references undeclared secret 'OTHER_TOKEN'"
        );
    }
}
