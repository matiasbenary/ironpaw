use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;

use crate::{
    message::ToolDefinition,
    scripts::ScriptRuntime,
    tools::{self, InternalTool, ToolContext, ToolRegistry},
};

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct CreateToolParams {
    /// Short snake_case name for the new tool, e.g. 'fetch_hn_top_stories'.
    pub name: String,
    /// One-line description of what the tool does, shown to whoever calls it.
    pub description: String,
    /// Tool version. Defaults to 0.1.0 when omitted.
    #[serde(default)]
    pub version: Option<String>,
    /// JavaScript source implementing the tool. The sandbox has no network
    /// or filesystem; the only way to act is the synchronous function
    /// `tool(name, args)`, which calls any existing tool and returns its
    /// JSON result. The tool's arguments are available as the global `args`,
    /// and the script must use `return` to produce the tool result. If the
    /// tool is meant to notify the user, the script must call send_message
    /// itself, with explicit channel and chat_id.
    /// Example: `const r = tool("web_fetch", { url: "https://example.com" });
    /// return { length: r.length };`
    pub script: String,
}

/// Lets the model author new tools as sandboxed scripts.
pub struct CreateToolTool {
    registry: Arc<ToolRegistry>,
    scripts: Arc<ScriptRuntime>,
    tools_dir: PathBuf,
}

impl CreateToolTool {
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

    pub async fn execute(&self, params: CreateToolParams) -> Result<serde_json::Value, String> {
        let name = params.name.trim().to_string();
        if !valid_tool_name(&name) {
            return Err("name must be non-empty snake_case (letters, digits, '_')".to_string());
        }
        if self.registry.contains(&name) {
            return Err(format!("a tool named '{name}' already exists"));
        }
        if params.script.trim().is_empty() {
            return Err("script must not be empty".to_string());
        }
        crate::scripts::validate_source(&params.script)?;
        let manifest = DynamicToolManifest {
            version: params.version.unwrap_or_else(|| "0.1.0".to_string()),
            description: params.description.clone(),
            parameters: None,
        };
        DynamicTool::write_files(&self.tools_dir, &name, &params.script, &manifest)?;
        self.registry.add(Arc::new(DynamicTool::new(
            name.clone(),
            params.description,
            None,
            params.script,
            Arc::clone(&self.scripts),
        )))?;
        Ok(serde_json::json!({ "ok": true, "name": name }))
    }
}

#[async_trait]
impl InternalTool for CreateToolTool {
    fn name(&self) -> &str {
        "create_tool"
    }

    fn tool_definition(&self) -> ToolDefinition {
        tools::tool_definition::<CreateToolParams>(
            self.name(),
            "Create a new tool implemented as a JavaScript script that can \
             call the existing tools. Use this when a task needs logic no \
             current tool covers (fetching and transforming data, multi-step \
             flows); the new tool can then be called directly or from an \
             automation.",
        )
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<(), String> {
        let output = tools::execute_typed(args, |params: CreateToolParams| async {
            CreateToolTool::execute(self, params).await
        })
        .await?;
        ctx.respond(output)?;
        Ok(())
    }
}

/// A tool implemented as a sandboxed script instead of Rust code. Dynamic
/// tools are created at runtime (by the model through `create_tool`, or
/// shipped as stock tools) and live in the same registry as internal tools.
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

    pub fn from_files(
        script_path: &Path,
        config_path: &Path,
        scripts: Arc<ScriptRuntime>,
    ) -> Result<Self, String> {
        let name = tool_name_from_script_path(script_path)?;
        let script = fs::read_to_string(script_path)
            .map_err(|err| format!("failed to read {}: {err}", script_path.display()))?;
        let manifest_json = fs::read_to_string(config_path)
            .map_err(|err| format!("failed to read {}: {err}", config_path.display()))?;
        let manifest: DynamicToolManifest = serde_json::from_str(&manifest_json)
            .map_err(|err| format!("invalid {}: {err}", config_path.display()))?;
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
            let script_path = entry.path();
            if script_path.extension().and_then(|ext| ext.to_str()) != Some("js") {
                continue;
            }
            let config_path = config_path_for_script(&script_path)?;
            tools.push(Self::from_files(
                &script_path,
                &config_path,
                Arc::clone(&scripts),
            )?);
        }
        Ok(tools)
    }

    pub fn write_files(
        dir: &Path,
        name: &str,
        script: &str,
        manifest: &DynamicToolManifest,
    ) -> Result<(), String> {
        fs::create_dir_all(dir)
            .map_err(|err| format!("failed to create {}: {err}", dir.display()))?;
        let script_path = dir.join(format!("{name}.js"));
        let config_path = dir.join(format!("{name}.config.json"));
        fs::write(&script_path, script)
            .map_err(|err| format!("failed to write {}: {err}", script_path.display()))?;
        let manifest_json =
            serde_json::to_string_pretty(manifest).map_err(|err| err.to_string())?;
        fs::write(&config_path, manifest_json)
            .map_err(|err| format!("failed to write {}: {err}", config_path.display()))
    }
}

fn tool_name_from_script_path(script_path: &Path) -> Result<String, String> {
    let name = script_path
        .file_stem()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("invalid dynamic tool filename: {}", script_path.display()))?;
    if !valid_tool_name(name) {
        return Err(format!(
            "dynamic tool filename must be snake_case: {}",
            script_path.display()
        ));
    }
    Ok(name.to_string())
}

fn config_path_for_script(script_path: &Path) -> Result<PathBuf, String> {
    let name = script_path
        .file_stem()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("invalid dynamic tool filename: {}", script_path.display()))?;
    Ok(script_path.with_file_name(format!("{name}.config.json")))
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

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<(), String> {
        let result = self
            .scripts
            .run(self.script.clone(), ctx.clone(), args)
            .await?;
        ctx.respond(result)?;
        Ok(())
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

    fn bound_create_tool() -> (Arc<InternalTools>, Arc<ScriptRuntime>, PathBuf) {
        let bus = Bus::new(4);
        let layout = crate::config::RuntimeLayout::from_root(std::env::temp_dir().join(format!(
            "warden-create-tool-test-runtime-{}",
            uuid::Uuid::new_v4()
        )));
        let files_dir = layout.files_dir();
        std::fs::create_dir_all(&files_dir).unwrap();
        let history = std::env::temp_dir().join(format!(
            "warden-create-tool-test-history-{}.db",
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

    #[tokio::test]
    async fn create_tool_writes_script_and_config() {
        let (tools, scripts, tools_dir) = bound_create_tool();

        let result = tools
            .create_tool
            .execute(CreateToolParams {
                name: "hello_dynamic".to_string(),
                description: "Say hello.".to_string(),
                version: Some("1.2.3".to_string()),
                script: "return { greeting: 'hello' };".to_string(),
            })
            .await
            .expect("tool should be created");

        assert_eq!(result["ok"], true);
        assert!(tools.registry.contains("hello_dynamic"));
        assert_eq!(
            std::fs::read_to_string(tools_dir.join("hello_dynamic.js")).unwrap(),
            "return { greeting: 'hello' };"
        );
        let manifest: DynamicToolManifest = serde_json::from_str(
            &std::fs::read_to_string(tools_dir.join("hello_dynamic.config.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest.version, "1.2.3");
        assert_eq!(manifest.description, "Say hello.");

        let discovered = DynamicTool::discover(&tools_dir, scripts).expect("discover tools");
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].name(), "hello_dynamic");
        assert_eq!(discovered[0].tool_definition().description, "Say hello.");
    }

    #[tokio::test]
    async fn create_tool_rejects_internal_tool_names() {
        let (tools, _scripts, tools_dir) = bound_create_tool();

        let result = tools
            .create_tool
            .execute(CreateToolParams {
                name: "send_message".to_string(),
                description: "Override send_message.".to_string(),
                version: None,
                script: "null".to_string(),
            })
            .await;

        assert!(result.is_err());
        assert!(!tools_dir.join("send_message.js").exists());
        assert!(!tools_dir.join("send_message.config.json").exists());
    }

    #[tokio::test]
    async fn create_tool_rejects_invalid_javascript_before_writing_files() {
        let (tools, _scripts, tools_dir) = bound_create_tool();

        let result = tools
            .create_tool
            .execute(CreateToolParams {
                name: "bad_script".to_string(),
                description: "Bad script.".to_string(),
                version: None,
                script: "const nope = ;".to_string(),
            })
            .await;

        assert!(result.is_err());
        assert!(!tools.registry.contains("bad_script"));
        assert!(!tools_dir.join("bad_script.js").exists());
        assert!(!tools_dir.join("bad_script.config.json").exists());
    }
}
