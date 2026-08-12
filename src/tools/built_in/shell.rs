use std::{sync::Arc, time::Duration};

use async_trait::async_trait;

use crate::{
    config::RuntimeLayout,
    message::ToolDefinition,
    shell_tools::{DEFAULT_COMMANDS, ShellExecutor, ShellToolManifest},
    tools::{self, ExecutionContext, InternalTool, ToolResponder, state::EnabledTools},
};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ShellParams {
    /// Installed shell-tool package containing the command.
    pub tool: String,
    /// Command exposed by that package's manifest.
    pub command: String,
    /// Arguments matching one of the command's allowed invocation shapes.
    #[serde(default)]
    pub args: Vec<String>,
    /// Relative directory inside the shell tool's private workspace.
    #[serde(default)]
    pub cwd: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ListShellCommandsParams {}

#[derive(Debug, serde::Serialize)]
struct ShellOutput {
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
    stdout_truncated: bool,
    stderr_truncated: bool,
    duration_ms: u64,
}

pub struct ShellTool {
    tools_dir: std::path::PathBuf,
    enabled: EnabledTools,
    executor: ShellExecutor,
}

pub struct ListShellCommandsTool {
    tools_dir: std::path::PathBuf,
    enabled: EnabledTools,
}

impl ListShellCommandsTool {
    pub fn new(layout: RuntimeLayout) -> Self {
        Self {
            tools_dir: layout.tools_dir(),
            enabled: EnabledTools::new(layout.tools_state_path()),
        }
    }

    fn list(&self) -> Result<serde_json::Value, String> {
        let mut packages = Vec::new();
        for manifest in ShellToolManifest::discover(&self.tools_dir)? {
            if self.enabled.is_enabled(&manifest.name)? {
                packages.push(serde_json::json!({
                    "tool": manifest.name,
                    "description": manifest.description,
                    "commands": manifest.commands,
                }));
            }
        }
        Ok(serde_json::json!({
            "default_commands": DEFAULT_COMMANDS,
            "packages": packages,
        }))
    }
}

impl ShellTool {
    pub fn new(layout: RuntimeLayout, executor: ShellExecutor) -> Self {
        Self {
            tools_dir: layout.tools_dir(),
            enabled: EnabledTools::new(layout.tools_state_path()),
            executor,
        }
    }

    async fn run(&self, params: ShellParams) -> Result<ShellOutput, String> {
        if !self.enabled.is_enabled(&params.tool)? {
            return Err(format!(
                "shell tool '{}' is not installed and enabled",
                params.tool
            ));
        }
        let manifest = Arc::new(ShellToolManifest::load_named(
            &self.tools_dir,
            &params.tool,
        )?);
        let output = self
            .executor
            .execute(
                manifest,
                params.command,
                params.args,
                params.cwd,
                COMMAND_TIMEOUT,
            )
            .await?;
        Ok(ShellOutput {
            stdout: output.stdout,
            stderr: output.stderr,
            exit_code: output.exit_code,
            stdout_truncated: output.stdout_truncated,
            stderr_truncated: output.stderr_truncated,
            duration_ms: output.duration.as_millis().min(u64::MAX as u128) as u64,
        })
    }

    fn description(&self) -> String {
        let mut description = format!(
            "Run one command inside an installed shell-tool package. Package commands use their \
             declared argument interfaces. The sandboxed default commands {:?} accept arbitrary \
             arguments but receive no secrets. Commands execute serially. Use list_shell_commands \
             to discover enabled packages and their command interfaces.",
            DEFAULT_COMMANDS
        );
        let Ok(manifests) = ShellToolManifest::discover(&self.tools_dir) else {
            return description;
        };
        for manifest in manifests {
            if !self.enabled.is_enabled(&manifest.name).unwrap_or(false) {
                continue;
            }
            let Some(skill) = manifest.skill else {
                continue;
            };
            let Ok(instructions) =
                std::fs::read_to_string(self.tools_dir.join(&manifest.name).join(skill))
            else {
                continue;
            };
            description.push_str(&format!(
                "\n\nInstructions for installed shell tool '{}':\n{}",
                manifest.name, instructions
            ));
        }
        description
    }
}

#[async_trait]
impl InternalTool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn tool_definition(&self) -> ToolDefinition {
        tools::tool_definition::<ShellParams>(self.name(), &self.description())
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ExecutionContext,
        responder: ToolResponder,
    ) {
        responder.respond(tools::execute_typed(args, |params| self.run(params)).await);
    }
}

#[async_trait]
impl InternalTool for ListShellCommandsTool {
    fn name(&self) -> &str {
        "list_shell_commands"
    }

    fn tool_definition(&self) -> ToolDefinition {
        tools::tool_definition::<ListShellCommandsParams>(
            self.name(),
            "List every command available through shell, grouped by enabled shell-tool package. \
             Default commands work in every listed package.",
        )
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _ctx: &ExecutionContext,
        responder: ToolResponder,
    ) {
        responder.respond(self.list().map_err(crate::tools::ToolError::new));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        secrets::Secrets,
        shell_tools::{
            InstallMethod, MANIFEST_FILENAME, ShellCommand, ShellInvocation, ShellToolManifest,
        },
    };

    fn install_test_tool(layout: &RuntimeLayout) {
        let tool_dir = layout.tools_dir().join("example");
        let bin_dir = tool_dir.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let executable = bin_dir.join("hello");
        std::fs::write(&executable, "#!/bin/sh\nprintf hello\n").unwrap();
        std::fs::write(tool_dir.join("SKILL.md"), "Always say hello.").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let manifest = ShellToolManifest {
            name: "example".to_string(),
            description: "Example shell tool".to_string(),
            install: InstallMethod::Preinstalled {
                name: "hello".to_string(),
            },
            commands: vec![ShellCommand {
                name: "hello".to_string(),
                git_credential_helper: None,
                invocations: vec![ShellInvocation {
                    args_prefix: Vec::new(),
                    secrets: Vec::new(),
                    options: Vec::new(),
                    min_positionals: 0,
                    max_positionals: Some(0),
                }],
            }],
            skill: Some("SKILL.md".to_string()),
            secrets: Vec::new(),
            allowed_hosts: Vec::new(),
        };
        std::fs::write(
            tool_dir.join(MANIFEST_FILENAME),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn runs_only_enabled_installed_shell_tools() {
        let root =
            std::env::temp_dir().join(format!("ironpaw-shell-tool-{}", uuid::Uuid::new_v4()));
        let layout = RuntimeLayout::from_root(root.clone());
        install_test_tool(&layout);
        let secrets = Secrets::for_tests();
        let executor = ShellExecutor::portable(layout.tools_dir(), secrets.tools);
        let tool = ShellTool::new(layout.clone(), executor);
        let commands = ListShellCommandsTool::new(layout.clone());

        assert_eq!(commands.list().unwrap()["packages"], serde_json::json!([]));

        let error = tool
            .run(ShellParams {
                tool: "example".to_string(),
                command: "hello".to_string(),
                args: Vec::new(),
                cwd: None,
            })
            .await
            .unwrap_err();
        assert!(error.contains("not installed and enabled"));

        EnabledTools::new(layout.tools_state_path())
            .enable("example")
            .unwrap();
        let listed = commands.list().unwrap();
        assert_eq!(listed["packages"][0]["tool"], "example");
        assert_eq!(listed["packages"][0]["commands"][0]["name"], "hello");
        assert!(
            listed["default_commands"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("cat"))
        );
        let output = tool
            .run(ShellParams {
                tool: "example".to_string(),
                command: "hello".to_string(),
                args: Vec::new(),
                cwd: None,
            })
            .await
            .unwrap();
        assert_eq!(output.stdout, "hello");
        assert!(
            tool.tool_definition()
                .description
                .contains("Always say hello.")
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
