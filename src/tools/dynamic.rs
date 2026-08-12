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
pub struct CreateToolParams {
    /// Tool name using letters, digits, underscores, or hyphens.
    pub name: String,
    /// One-line description shown to callers of this tool.
    pub description: String,
    /// Optional JSON Schema for the tool arguments. Defaults to a free-form object.
    #[serde(default)]
    pub parameters: Option<serde_json::Value>,
    /// JavaScript source. Use top-level code and return the tool result.
    pub script: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct DeleteToolParams {
    /// Exact dynamic tool name to delete.
    pub name: String,
}

/// Creates, updates, or deletes a sandboxed JavaScript tool.
pub struct DynamicToolTool {
    registry: Arc<ToolRegistry>,
    scripts: Arc<ScriptRuntime>,
    tools_dir: PathBuf,
    kind: DynamicToolKind,
}

#[derive(Clone, Copy)]
pub enum DynamicToolKind {
    Create,
    Edit,
    Delete,
}

impl DynamicToolTool {
    pub fn new(
        registry: Arc<ToolRegistry>,
        scripts: Arc<ScriptRuntime>,
        tools_dir: PathBuf,
        kind: DynamicToolKind,
    ) -> Self {
        Self {
            registry,
            scripts,
            tools_dir,
            kind,
        }
    }

    fn save(
        &self,
        params: CreateToolParams,
        must_exist: bool,
    ) -> Result<serde_json::Value, String> {
        let name = params.name.trim().to_string();
        if !valid_tool_name(&name) {
            return Err(
                "name must be non-empty and use only ASCII letters, digits, underscores, or hyphens"
                .to_string(),
            );
        }
        if self.registry.contains(&name) != must_exist {
            return Err(if must_exist {
                format!("unknown dynamic tool: {name}")
            } else {
                format!("a tool named '{name}' already exists")
            });
        }
        if params.script.trim().is_empty() {
            return Err("script must not be empty".to_string());
        }
        self.scripts.validate_source(&params.script)?;
        let directory = self.tools_dir.join(&name);
        let manifest = DynamicToolManifest {
            description: params.description.clone(),
            parameters: params.parameters.clone(),
        };
        DynamicTool::write_files(&directory, &params.script, &manifest)?;
        let tool = DynamicTool::new(
            name.clone(),
            params.description,
            params.parameters,
            params.script,
            self.scripts.clone(),
        );
        self.registry.register_dynamic(Arc::new(tool))?;
        Ok(serde_json::json!({ "name": name }))
    }

    fn delete(&self, params: DeleteToolParams) -> Result<serde_json::Value, String> {
        if !self.registry.remove_dynamic(&params.name)? {
            return Err(format!("unknown dynamic tool: {}", params.name));
        }
        let directory = self.tools_dir.join(&params.name);
        fs::remove_dir_all(&directory)
            .map_err(|err| format!("failed to remove {}: {err}", directory.display()))?;
        Ok(serde_json::json!({ "name": params.name }))
    }
}

#[async_trait]
impl InternalTool for DynamicToolTool {
    fn name(&self) -> &str {
        match self.kind {
            DynamicToolKind::Create => "create_tool",
            DynamicToolKind::Edit => "edit_tool",
            DynamicToolKind::Delete => "delete_tool",
        }
    }

    fn tool_definition(&self) -> ToolDefinition {
        match self.kind {
            DynamicToolKind::Create => tools::tool_definition::<CreateToolParams>(
                self.name(),
                "Create and activate a JavaScript tool. Write top-level script code and return its \
                 result; do not define an uncalled main function. Arguments are available as `args`. \
                 Registered tools are callable globals: use web_fetch({ url }), never fetch(url).",
            ),
            DynamicToolKind::Edit => tools::tool_definition::<CreateToolParams>(
                self.name(),
                "Replace an existing dynamic JavaScript tool and activate the complete replacement. \
                 Pass its complete script, description, and optional parameter schema.",
            ),
            DynamicToolKind::Delete => tools::tool_definition::<DeleteToolParams>(
                self.name(),
                "Delete an existing dynamic tool and its stored files.",
            ),
        }
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ExecutionContext,
        responder: ToolResponder,
    ) {
        let result = match self.kind {
            DynamicToolKind::Create => serde_json::from_value(args)
                .map_err(|err| err.to_string())
                .and_then(|params| self.save(params, false)),
            DynamicToolKind::Edit => serde_json::from_value(args)
                .map_err(|err| err.to_string())
                .and_then(|params| self.save(params, true)),
            DynamicToolKind::Delete => serde_json::from_value::<DeleteToolParams>(args)
                .map_err(|err| err.to_string())
                .and_then(|params| self.delete(params)),
        };
        responder.respond(result.map_err(crate::tools::ToolError::new));
    }
}

/// A tool implemented as a sandboxed script instead of Rust code. Dynamic
/// tools are created and edited by typed tool calls and live in the same
/// registry as internal tools.
pub struct DynamicTool {
    name: String,
    description: String,
    parameters: serde_json::Value,
    script: String,
    scripts: Arc<ScriptRuntime>,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct DynamicToolManifest {
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
        scripts.validate_source(&script)?;
        Ok(Self::new(
            name,
            manifest.description,
            manifest.parameters,
            script,
            scripts,
        ))
    }

    fn write_files(dir: &Path, script: &str, manifest: &DynamicToolManifest) -> Result<(), String> {
        fs::create_dir_all(dir)
            .map_err(|err| format!("failed to create {}: {err}", dir.display()))?;
        let script_path = dir.join("tool.js");
        fs::write(&script_path, script)
            .map_err(|err| format!("failed to write {}: {err}", script_path.display()))?;
        let config_path = dir.join("config.json");
        let contents = serde_json::to_string_pretty(manifest).map_err(|err| err.to_string())?;
        fs::write(&config_path, format!("{contents}\n"))
            .map_err(|err| format!("failed to write {}: {err}", config_path.display()))
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
            "dynamic tool directory must use letters, digits, underscores, or hyphens: {}",
            dir.display()
        ));
    }
    Ok(name.to_string())
}

pub fn valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
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
        inference::Inference,
        tools::{InternalTools, messages::SendMessageTool},
    };

    struct DynamicHarness {
        tools: Arc<InternalTools>,
        create_tool: DynamicToolTool,
        edit_tool: DynamicToolTool,
        delete_tool: DynamicToolTool,
        scripts: Arc<ScriptRuntime>,
        tools_dir: PathBuf,
    }

    fn bound_dynamic_tools() -> DynamicHarness {
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
            Inference::new(bus.clone(), secrets.tools.clone()).unwrap(),
            SendMessageTool::new(bus.clone(), |_, _| Err("no attachments".to_string())),
            crate::tools::messages::DeleteMessageTool::new(bus),
            files_dir,
            Arc::new(SqliteHistory::new(history).unwrap()),
            secrets,
        ));
        let scripts = Arc::clone(&tools.scripts);
        let tools_dir = layout.tools_dir();
        let dynamic_tool = |kind| {
            DynamicToolTool::new(
                Arc::clone(&tools.registry),
                Arc::clone(&scripts),
                tools_dir.clone(),
                kind,
            )
        };
        let create_tool = dynamic_tool(DynamicToolKind::Create);
        let edit_tool = dynamic_tool(DynamicToolKind::Edit);
        let delete_tool = dynamic_tool(DynamicToolKind::Delete);
        DynamicHarness {
            tools,
            create_tool,
            edit_tool,
            delete_tool,
            scripts,
            tools_dir,
        }
    }

    #[test]
    fn create_tool_writes_and_registers_files() {
        let harness = bound_dynamic_tools();
        let tool_dir = harness.tools_dir.join("hello_dynamic");

        let result = harness
            .create_tool
            .save(
                CreateToolParams {
                    name: "hello_dynamic".to_string(),
                    description: "Say hello.".to_string(),
                    parameters: Some(serde_json::json!({
                        "type": "object",
                        "properties": { "name": { "type": "string" } }
                    })),
                    script: "return { greeting: 'hello' };".to_string(),
                },
                false,
            )
            .expect("tool should be registered");

        assert_eq!(result["name"], "hello_dynamic");
        assert!(harness.tools.registry.contains("hello_dynamic"));
        let manifest: DynamicToolManifest =
            serde_json::from_str(&std::fs::read_to_string(tool_dir.join("config.json")).unwrap())
                .unwrap();
        assert_eq!(manifest.description, "Say hello.");
        assert_eq!(
            manifest.parameters,
            Some(serde_json::json!({
                "type": "object",
                "properties": { "name": { "type": "string" } }
            }))
        );

        let discovered =
            DynamicTool::discover(&harness.tools_dir, harness.scripts).expect("discover tools");
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].name(), "hello_dynamic");
        assert_eq!(discovered[0].tool_definition().description, "Say hello.");
    }

    #[test]
    fn edit_and_delete_tool_replace_the_active_tool_and_files() {
        let harness = bound_dynamic_tools();
        harness
            .create_tool
            .save(
                CreateToolParams {
                    name: "weather".to_string(),
                    description: "Old weather tool.".to_string(),
                    parameters: None,
                    script: "return { temperature: 20 };".to_string(),
                },
                false,
            )
            .unwrap();

        harness
            .edit_tool
            .save(
                CreateToolParams {
                    name: "weather".to_string(),
                    description: "New weather tool.".to_string(),
                    parameters: None,
                    script: "return { temperature: 21 };".to_string(),
                },
                true,
            )
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(harness.tools_dir.join("weather/tool.js")).unwrap(),
            "return { temperature: 21 };"
        );

        harness
            .delete_tool
            .delete(DeleteToolParams {
                name: "weather".to_string(),
            })
            .unwrap();
        assert!(!harness.tools.registry.contains("weather"));
        assert!(!harness.tools_dir.join("weather").exists());
    }

    #[test]
    fn create_tool_rejects_internal_tool_names() {
        let harness = bound_dynamic_tools();

        let result = harness.create_tool.save(
            CreateToolParams {
                name: "send_message".to_string(),
                description: "Override send_message.".to_string(),
                parameters: None,
                script: "return null;".to_string(),
            },
            false,
        );

        assert!(result.is_err());
        assert!(harness.tools.registry.contains("send_message"));
    }
}
