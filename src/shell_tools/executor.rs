use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use tokio::sync::{mpsc, oneshot};

use crate::secrets::ToolSecrets;

use super::{
    DEFAULT_COMMANDS, ShellCommand, ShellInvocation, ShellToolManifest,
    process::{ProcessMode, ProcessOutput, ProcessRequest, ProcessRunner},
};

const QUEUE_CAPACITY: usize = 32;

#[derive(Clone, Copy)]
enum Isolation {
    Production,
    #[cfg(debug_assertions)]
    Portable,
}

#[derive(Clone)]
pub struct ShellExecutor {
    tools_dir: PathBuf,
    secrets: ToolSecrets,
    isolation: Isolation,
    sender: Arc<Mutex<Option<mpsc::Sender<Operation>>>>,
}

enum Operation {
    Execute {
        manifest: Arc<ShellToolManifest>,
        workspace: PathBuf,
        command: String,
        args: Vec<String>,
        working_dir: Option<String>,
        timeout: Duration,
        response: oneshot::Sender<Result<ProcessOutput, String>>,
    },
    Install {
        manifest: Arc<ShellToolManifest>,
        source_dir: PathBuf,
        response: oneshot::Sender<Result<(), String>>,
    },
}

impl ShellExecutor {
    pub fn new(tools_dir: PathBuf, secrets: ToolSecrets) -> Self {
        Self::start(tools_dir, secrets, Isolation::Production)
    }

    #[cfg(debug_assertions)]
    pub fn portable(tools_dir: PathBuf, secrets: ToolSecrets) -> Self {
        Self::start(tools_dir, secrets, Isolation::Portable)
    }

    fn start(tools_dir: PathBuf, secrets: ToolSecrets, isolation: Isolation) -> Self {
        Self {
            tools_dir,
            secrets,
            isolation,
            sender: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn execute(
        &self,
        manifest: Arc<ShellToolManifest>,
        workspace: PathBuf,
        command: String,
        args: Vec<String>,
        working_dir: Option<String>,
        timeout: Duration,
    ) -> Result<ProcessOutput, String> {
        let sender = self.sender();
        let (response, result) = oneshot::channel();
        sender
            .send(Operation::Execute {
                manifest,
                workspace,
                command,
                args,
                working_dir,
                timeout,
                response,
            })
            .await
            .map_err(|_| "shell executor stopped".to_string())?;
        result
            .await
            .map_err(|_| "shell executor stopped before returning a result".to_string())?
    }

    pub async fn install(
        &self,
        manifest: Arc<ShellToolManifest>,
        source_dir: PathBuf,
    ) -> Result<(), String> {
        let sender = self.sender();
        let (response, result) = oneshot::channel();
        sender
            .send(Operation::Install {
                manifest,
                source_dir,
                response,
            })
            .await
            .map_err(|_| "shell executor stopped".to_string())?;
        result
            .await
            .map_err(|_| "shell executor stopped before returning a result".to_string())?
    }

    fn sender(&self) -> mpsc::Sender<Operation> {
        let mut sender = self.sender.lock().expect("shell executor lock");
        if let Some(sender) = sender.as_ref() {
            return sender.clone();
        }
        let (new_sender, mut receiver) = mpsc::channel::<Operation>(QUEUE_CAPACITY);
        let tools_dir = self.tools_dir.clone();
        let secrets = self.secrets.clone();
        let isolation = self.isolation;
        tokio::spawn(async move {
            while let Some(operation) = receiver.recv().await {
                match operation {
                    Operation::Execute {
                        manifest,
                        workspace,
                        command,
                        args,
                        working_dir,
                        timeout,
                        response,
                    } => {
                        let result = run(
                            manifest,
                            workspace,
                            &command,
                            args,
                            working_dir,
                            timeout,
                            &tools_dir,
                            &secrets,
                            isolation,
                        )
                        .await;
                        let _ = response.send(result);
                    }
                    Operation::Install {
                        manifest,
                        source_dir,
                        response,
                    } => {
                        let result = install(manifest, source_dir, &tools_dir, isolation).await;
                        let _ = response.send(result);
                    }
                }
            }
        });
        *sender = Some(new_sender.clone());
        new_sender
    }
}

async fn install(
    manifest: Arc<ShellToolManifest>,
    source_dir: PathBuf,
    tools_dir: &std::path::Path,
    isolation: Isolation,
) -> Result<(), String> {
    manifest.validate()?;
    let tool_dir = tools_dir.join(&manifest.name);
    if tool_dir.exists() {
        return Err(format!(
            "shell tool '{}' is already installed",
            manifest.name
        ));
    }
    std::fs::create_dir_all(&tool_dir).map_err(|error| error.to_string())?;
    let runner = match isolation {
        Isolation::Production => ProcessRunner::new(
            tool_dir.clone(),
            tool_dir.clone(),
            manifest.allowed_hosts.clone(),
            ProcessMode::Install,
        ),
        #[cfg(debug_assertions)]
        Isolation::Portable => ProcessRunner::portable(
            tool_dir.clone(),
            tool_dir.clone(),
            manifest.allowed_hosts.clone(),
            ProcessMode::Install,
        ),
    };
    let result = async {
        let runner = runner?;
        super::install::install(&manifest, &source_dir, &tool_dir, &runner).await
    }
    .await;
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&tool_dir);
    }
    result
}

async fn run(
    manifest: Arc<ShellToolManifest>,
    workspace: PathBuf,
    command_name: &str,
    args: Vec<String>,
    working_dir: Option<String>,
    timeout: Duration,
    tools_dir: &std::path::Path,
    secrets: &ToolSecrets,
    isolation: Isolation,
) -> Result<ProcessOutput, String> {
    manifest.validate()?;
    let tool_dir = tools_dir.join(&manifest.name);
    let (executable, env) = if let Some(command) = manifest
        .commands
        .iter()
        .find(|command| command.name == command_name)
    {
        let invocation = validate_args(command, &args)?;
        let secrets = manifest
            .secrets
            .iter()
            .filter(|secret| invocation.secrets.contains(&secret.name))
            .map(|secret| {
                secrets
                    .get(&manifest.secret_store_name(&secret.name))
                    .filter(|value| !value.is_empty())
                    .map(|value| (secret.env.clone(), value))
                    .ok_or_else(|| format!("required secret '{}' is unavailable", secret.name))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut env = command
            .environment
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<Vec<_>>();
        env.extend(secrets);
        (tool_dir.join("bin").join(command_name), env)
    } else if DEFAULT_COMMANDS.contains(&command_name) {
        if args.iter().any(|arg| arg.contains('\0')) {
            return Err(format!(
                "arguments for '{command_name}' contain a null byte"
            ));
        }
        (system_command(command_name)?, Vec::new())
    } else {
        return Err(format!(
            "shell tool '{}' does not expose '{command_name}'",
            manifest.name
        ));
    };
    let runner = match isolation {
        Isolation::Production => ProcessRunner::new(
            tool_dir,
            workspace,
            manifest.allowed_hosts.clone(),
            ProcessMode::Execute,
        )?,
        #[cfg(debug_assertions)]
        Isolation::Portable => ProcessRunner::portable(
            tool_dir,
            workspace,
            manifest.allowed_hosts.clone(),
            ProcessMode::Execute,
        )?,
    };
    runner
        .run(ProcessRequest {
            executable,
            args,
            env,
            working_dir: working_dir.map(PathBuf::from),
            timeout,
        })
        .await
}

fn system_command(name: &str) -> Result<PathBuf, String> {
    ["/usr/local/bin", "/usr/bin", "/bin", "/opt/homebrew/bin"]
        .into_iter()
        .map(|directory| Path::new(directory).join(name))
        .find(|path| path.is_file())
        .ok_or_else(|| format!("default shell command '{name}' is not installed"))
}

fn validate_args<'a>(
    command: &'a ShellCommand,
    args: &[String],
) -> Result<&'a ShellInvocation, String> {
    if args.iter().any(|arg| arg.contains('\0')) {
        return Err(format!(
            "arguments for '{}' contain a null byte",
            command.name
        ));
    }
    if let Some(invocation) = command
        .invocations
        .iter()
        .find(|invocation| invocation_accepts(invocation, args))
    {
        return Ok(invocation);
    }
    let interface = serde_json::to_string(&command.invocations)
        .map_err(|error| format!("failed to describe command interface: {error}"))?;
    Err(format!(
        "arguments do not match an allowed invocation of '{}'; allowed interface: {interface}",
        command.name
    ))
}

fn invocation_accepts(invocation: &ShellInvocation, args: &[String]) -> bool {
    if !args.starts_with(&invocation.args_prefix) {
        return false;
    }
    let mut positionals = 0;
    let mut options_enabled = true;
    let mut used_options = std::collections::HashSet::new();
    let mut index = invocation.args_prefix.len();
    while index < args.len() {
        let arg = &args[index];
        if options_enabled && arg == "--" {
            options_enabled = false;
            index += 1;
            continue;
        }
        if options_enabled && arg.starts_with('-') {
            let Some(option) = invocation.options.iter().find(|option| option.name == *arg) else {
                return false;
            };
            if !option.repeatable && !used_options.insert(&option.name) {
                return false;
            }
            index += 1;
            if option.takes_value {
                if index == args.len() {
                    return false;
                }
                index += 1;
            }
            continue;
        }
        positionals += 1;
        index += 1;
    }
    positionals >= invocation.min_positionals
        && invocation
            .max_positionals
            .is_none_or(|maximum| positionals <= maximum)
}

#[cfg(test)]
mod tests {
    use std::{fs, time::Instant};

    use super::*;
    use crate::{
        secrets::Secrets,
        shell_tools::{InstallMethod, ShellOption, ShellSecret},
    };

    fn manifest(script_name: &str) -> ShellToolManifest {
        ShellToolManifest {
            name: "example".to_string(),
            description: "Test shell execution".to_string(),
            install: InstallMethod::Preinstalled {
                name: script_name.to_string(),
            },
            commands: vec![ShellCommand {
                name: script_name.to_string(),
                environment: std::collections::BTreeMap::new(),
                invocations: vec![ShellInvocation {
                    args_prefix: Vec::new(),
                    secrets: Vec::new(),
                    options: Vec::new(),
                    min_positionals: 0,
                    max_positionals: Some(0),
                }],
            }],
            skill: None,
            secrets: Vec::new(),
            allowed_hosts: Vec::new(),
        }
    }

    fn install_script(tools_dir: &std::path::Path, name: &str, contents: &str) {
        let bin_dir = tools_dir.join("example/bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let path = bin_dir.join(name);
        fs::write(&path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    #[test]
    fn validates_declared_command_interface() {
        let command = ShellCommand {
            name: "gh".to_string(),
            environment: std::collections::BTreeMap::new(),
            invocations: vec![ShellInvocation {
                args_prefix: vec!["issue".to_string(), "list".to_string()],
                secrets: Vec::new(),
                options: vec![ShellOption {
                    name: "--repo".to_string(),
                    takes_value: true,
                    repeatable: false,
                }],
                min_positionals: 0,
                max_positionals: Some(0),
            }],
        };
        assert!(validate_args(&command, &["issue".into(), "list".into()]).is_ok());
        assert!(
            validate_args(
                &command,
                &[
                    "issue".into(),
                    "list".into(),
                    "--repo".into(),
                    "owner/repo".into()
                ]
            )
            .is_ok()
        );
        let error =
            validate_args(&command, &["issue".into(), "delete".into(), "123".into()]).unwrap_err();
        assert!(error.contains("allowed interface"));
        assert!(error.contains("--repo"));
    }

    #[tokio::test]
    async fn injects_only_declared_secrets() {
        let tools_dir =
            std::env::temp_dir().join(format!("ironpaw-executor-{}", uuid::Uuid::new_v4()));
        install_script(
            &tools_dir,
            "reveal",
            "#!/bin/sh\nprintf '%s:%s:%s' \"$TOKEN\" \"${UNDECLARED-unset}\" \"$STATIC\"\n",
        );
        let secrets = Secrets::for_tests();
        secrets.set("stored-token", "other-tool-secret").unwrap();
        let mut manifest = manifest("reveal");
        manifest.commands[0]
            .environment
            .insert("STATIC".to_string(), "configured".to_string());
        manifest.secrets.push(ShellSecret {
            name: "stored-token".to_string(),
            env: "TOKEN".to_string(),
        });
        manifest.commands[0].invocations[0]
            .secrets
            .push("stored-token".to_string());
        secrets
            .set(&manifest.secret_store_name("stored-token"), "secret")
            .unwrap();
        let executor = ShellExecutor::portable(tools_dir.clone(), secrets.tools);
        let output = executor
            .execute(
                Arc::new(manifest.clone()),
                tools_dir.join("workspace"),
                "reveal".to_string(),
                Vec::new(),
                None,
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(output.stdout, "secret:unset:configured");

        manifest.commands[0].invocations[0].secrets.clear();
        let output = executor
            .execute(
                Arc::new(manifest),
                tools_dir.join("workspace"),
                "reveal".to_string(),
                Vec::new(),
                None,
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(output.stdout, ":unset:configured");
        fs::remove_dir_all(tools_dir).unwrap();
    }

    #[tokio::test]
    async fn executes_commands_serially() {
        let tools_dir =
            std::env::temp_dir().join(format!("ironpaw-executor-{}", uuid::Uuid::new_v4()));
        install_script(&tools_dir, "pause", "#!/bin/sh\nsleep 0.05\n");
        let executor = ShellExecutor::portable(tools_dir.clone(), Secrets::for_tests().tools);
        let manifest = Arc::new(manifest("pause"));
        let started = Instant::now();
        let first = executor.execute(
            Arc::clone(&manifest),
            tools_dir.join("workspace"),
            "pause".to_string(),
            Vec::new(),
            None,
            Duration::from_secs(5),
        );
        let second = executor.execute(
            manifest,
            tools_dir.join("workspace"),
            "pause".to_string(),
            Vec::new(),
            None,
            Duration::from_secs(5),
        );
        let (first, second) = tokio::join!(first, second);
        first.unwrap();
        second.unwrap();
        assert!(started.elapsed() >= Duration::from_millis(90));
        fs::remove_dir_all(tools_dir).unwrap();
    }

    #[tokio::test]
    async fn installs_preinstalled_command_and_skill() {
        let tools_dir =
            std::env::temp_dir().join(format!("ironpaw-executor-{}", uuid::Uuid::new_v4()));
        let source_dir =
            std::env::temp_dir().join(format!("ironpaw-shell-source-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&source_dir).unwrap();
        fs::write(source_dir.join("SKILL.md"), "Use sh carefully.").unwrap();
        let mut manifest = manifest("sh");
        manifest.skill = Some("SKILL.md".to_string());
        let manifest = Arc::new(manifest);
        let executor = ShellExecutor::portable(tools_dir.clone(), Secrets::for_tests().tools);

        executor
            .install(Arc::clone(&manifest), source_dir.clone())
            .await
            .unwrap();
        assert!(tools_dir.join("example/bin/sh").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            for path in [
                tools_dir.join("example"),
                tools_dir.join("example/bin"),
                tools_dir.join("example/shell.json"),
                tools_dir.join("example/SKILL.md"),
            ] {
                assert_eq!(
                    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o222,
                    0
                );
            }
        }
        assert_eq!(
            fs::read_to_string(tools_dir.join("example/SKILL.md")).unwrap(),
            "Use sh carefully."
        );
        assert_eq!(
            ShellToolManifest::load_named(&tools_dir, "example")
                .unwrap()
                .name,
            "example"
        );
        let output = executor
            .execute(
                manifest,
                tools_dir.join("workspace"),
                "sh".to_string(),
                Vec::new(),
                None,
                Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(output.exit_code, Some(0));

        #[cfg(unix)]
        for path in [tools_dir.join("example"), tools_dir.join("example/bin")] {
            use std::os::unix::fs::PermissionsExt as _;

            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(permissions.mode() | 0o700);
            fs::set_permissions(path, permissions).unwrap();
        }
        fs::remove_dir_all(tools_dir).unwrap();
        fs::remove_dir_all(source_dir).unwrap();
    }
}
