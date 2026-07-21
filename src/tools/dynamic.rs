use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;

use crate::{
    message::ToolDefinition,
    scripts::ScriptRuntime,
    tools::{self, ExecutionContext, InternalTool, ToolRegistry, ToolResponder},
};

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct RegisterToolParams {
    /// Name of the tool directory under tools/ to validate and register.
    pub name: String,
}

/// Validates and registers a file-backed sandboxed JavaScript tool.
pub struct RegisterToolTool {
    registry: Arc<ToolRegistry>,
    scripts: Arc<ScriptRuntime>,
    tools_dir: PathBuf,
}

impl RegisterToolTool {
    pub fn new(
        registry: Arc<ToolRegistry>,
        scripts: Arc<ScriptRuntime>,
        tools_dir: PathBuf,
    ) -> Self {
        Self {
            registry,
            scripts,
            tools_dir,
        }
    }

    pub fn execute(&self, params: RegisterToolParams) -> Result<serde_json::Value, String> {
        let name = params.name.trim().to_string();
        if !valid_tool_name(&name) {
            return Err("name must be non-empty snake_case (letters, digits, '_')".to_string());
        }
        let tool = DynamicTool::from_directory(&self.tools_dir.join(&name), self.scripts.clone())?;
        self.registry.register_dynamic(Arc::new(tool))?;
        Ok(serde_json::json!({ "name": name }))
    }
}

#[async_trait]
impl InternalTool for RegisterToolTool {
    fn name(&self) -> &str {
        "register_tool"
    }

    fn tool_definition(&self) -> ToolDefinition {
        tools::tool_definition::<RegisterToolParams>(
            self.name(),
            "Validate and activate the JavaScript tool in tools/<name>/tool.js \
             with its tools/<name>/config.json. config.json requires `version` \
             and `description`; it may contain `parameters`, a JSON Schema for \
             the tool arguments. The script receives `args` and may call existing \
             tools as globals. Changes do not take effect until this tool is called.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ExecutionContext,
        responder: ToolResponder,
    ) {
        let result = serde_json::from_value(args)
            .map_err(|err| err.to_string())
            .and_then(|params| RegisterToolTool::execute(self, params));
        responder.respond(result.map_err(crate::tools::ToolError::new));
    }
}

/// A tool implemented as a sandboxed script instead of Rust code. Dynamic
/// tools are activated by `register_tool` and live in the same registry as
/// internal tools.
pub struct DynamicTool {
    name: String,
    description: String,
    parameters: serde_json::Value,
    script: String,
    scripts: Arc<ScriptRuntime>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct DynamicToolManifest {
    pub version: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
}

impl DynamicTool {
    pub fn new(
        name: String,
        description: String,
        parameters: Option<serde_json::Value>,
        script: String,
        scripts: Arc<ScriptRuntime>,
    ) -> Self {
        Self {
            name,
            description,
            parameters: parameters.unwrap_or_else(free_form_parameters),
            script,
            scripts,
        }
    }

    pub fn from_directory(dir: &Path, scripts: Arc<ScriptRuntime>) -> Result<Self, String> {
        let name = tool_name_from_directory(dir)?;
        let script_path = dir.join("tool.js");
        let config_path = dir.join("config.json");
        let script = fs::read_to_string(&script_path)
            .map_err(|err| format!("failed to read {}: {err}", script_path.display()))?;
        let manifest_json = fs::read_to_string(&config_path)
            .map_err(|err| format!("failed to read {}: {err}", config_path.display()))?;
        let manifest: DynamicToolManifest = serde_json::from_str(&manifest_json)
            .map_err(|err| format!("invalid {}: {err}", config_path.display()))?;
        crate::scripts::validate_source(&script)?;
        Ok(Self::new(
            name,
            manifest.description,
            manifest.parameters,
            script,
            scripts,
        ))
    }

    pub fn discover(dir: &Path, scripts: Arc<ScriptRuntime>) -> Result<Vec<Self>, String> {
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let entries =
            fs::read_dir(dir).map_err(|err| format!("failed to read {}: {err}", dir.display()))?;
        let mut tools = Vec::new();
        for entry in entries {
            let entry =
                entry.map_err(|err| format!("failed to read {} entry: {err}", dir.display()))?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            tools.push(Self::from_directory(&path, Arc::clone(&scripts))?);
        }
        Ok(tools)
    }
}

fn tool_name_from_directory(dir: &Path) -> Result<String, String> {
    let name = dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("invalid dynamic tool directory: {}", dir.display()))?;
    if !valid_tool_name(name) {
        return Err(format!(
            "dynamic tool directory must be snake_case: {}",
            dir.display()
        ));
    }
    Ok(name.to_string())
}

pub fn valid_tool_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn free_form_parameters() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": true
    })
}

#[async_trait]
impl InternalTool for DynamicTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn tool_definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.parameters.clone(),
        }
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ExecutionContext,
        responder: ToolResponder,
    ) {
        let result = self
            .scripts
            .run(self.script.clone(), ctx.clone(), args)
            .await;
        responder.respond(result.map_err(crate::tools::ToolError::new));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bus::Bus,
        history::SqliteHistory,
        tools::{InternalTools, messages::SendMessageTool, raw_inference::RawInferenceTool},
    };

    fn bound_register_tool() -> (Arc<InternalTools>, Arc<ScriptRuntime>, PathBuf) {
        let bus = Bus::new(4);
        let layout = crate::config::RuntimeLayout::from_root(std::env::temp_dir().join(format!(
            "ironpaw-create-tool-test-runtime-{}",
            uuid::Uuid::new_v4()
        )));
        let files_dir = layout.files_dir();
        std::fs::create_dir_all(&files_dir).unwrap();
        let history = std::env::temp_dir().join(format!(
            "ironpaw-create-tool-test-history-{}.db",
            uuid::Uuid::new_v4()
        ));
        let secrets = crate::secrets::Secrets::for_tests();
        let tools = Arc::new(InternalTools::new(
            bus.clone(),
            layout.clone(),
            RawInferenceTool::new(secrets.tools.clone()).unwrap(),
            SendMessageTool::new(bus.clone(), |_| Err("no attachments".to_string())),
            crate::tools::messages::DeleteMessageTool::new(bus),
            files_dir,
            Arc::new(SqliteHistory::new(history).unwrap()),
            secrets,
        ));
        let scripts = Arc::clone(&tools.scripts);
        let tools_dir = layout.tools_dir();
        (tools, scripts, tools_dir)
    }

    #[test]
    fn register_tool_loads_directory_files() {
        let (tools, scripts, tools_dir) = bound_register_tool();
        let tool_dir = tools_dir.join("hello_dynamic");
        std::fs::create_dir_all(&tool_dir).unwrap();
        std::fs::write(tool_dir.join("tool.js"), "return { greeting: 'hello' };").unwrap();
        std::fs::write(
            tool_dir.join("config.json"),
            r#"{"version":"1.2.3","description":"Say hello."}"#,
        )
        .unwrap();

        let result = tools
            .register_tool
            .execute(RegisterToolParams {
                name: "hello_dynamic".to_string(),
            })
            .expect("tool should be registered");

        assert_eq!(result["name"], "hello_dynamic");
        assert!(tools.registry.contains("hello_dynamic"));
        let manifest: DynamicToolManifest =
            serde_json::from_str(&std::fs::read_to_string(tool_dir.join("config.json")).unwrap())
                .unwrap();
        assert_eq!(manifest.version, "1.2.3");
        assert_eq!(manifest.description, "Say hello.");

        let discovered = DynamicTool::discover(&tools_dir, scripts).expect("discover tools");
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].name(), "hello_dynamic");
        assert_eq!(discovered[0].tool_definition().description, "Say hello.");
    }

    #[test]
    fn register_tool_rejects_internal_tool_names() {
        let (tools, _scripts, tools_dir) = bound_register_tool();
        let tool_dir = tools_dir.join("send_message");
        std::fs::create_dir_all(&tool_dir).unwrap();
        std::fs::write(tool_dir.join("tool.js"), "return null;").unwrap();
        std::fs::write(
            tool_dir.join("config.json"),
            r#"{"version":"0.1.0","description":"Override send_message."}"#,
        )
        .unwrap();

        let result = tools.register_tool.execute(RegisterToolParams {
            name: "send_message".to_string(),
        });

        assert!(result.is_err());
        assert!(tools.registry.contains("send_message"));
    }

    #[test]
    fn register_tool_rejects_invalid_javascript() {
        let (tools, _scripts, tools_dir) = bound_register_tool();
        let tool_dir = tools_dir.join("bad_script");
        std::fs::create_dir_all(&tool_dir).unwrap();
        std::fs::write(tool_dir.join("tool.js"), "const nope = ;").unwrap();
        std::fs::write(
            tool_dir.join("config.json"),
            r#"{"version":"0.1.0","description":"Bad script."}"#,
        )
        .unwrap();

        let result = tools.register_tool.execute(RegisterToolParams {
            name: "bad_script".to_string(),
        });

        assert!(result.is_err());
        assert!(!tools.registry.contains("bad_script"));
    }
}
