use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;

use crate::{
    message::ToolDefinition,
    scripts::ScriptRuntime,
    tools::{InternalTool, ToolContext, ToolOutput},
};

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

    /// The stock tool behind warden's default automation: run the agent on
    /// the current conversation and deliver its answer back there.
    pub fn handle_conversation(scripts: Arc<ScriptRuntime>) -> Self {
        const SCRIPT: &str = r#"
	const r = tool("continue_conversation", { conversation_id: args.conversation_id });
	if (r.ok && r.result) {
	    tool("send_message", { content: r.result });
	    return { ok: true };
	} else if (!r.ok) {
	    tool("send_message", { content: "agent failed: " + r.error });
	    return r;
	}
	return { ok: true };
	"#;
        Self::new(
            "handle_conversation".to_string(),
            "Handle a user message: run the agent on it and deliver the \
             answer back to the conversation it came from."
                .to_string(),
            Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "conversation_id": { "type": "string" }
                },
                "required": ["conversation_id"],
                "additionalProperties": false
            })),
            SCRIPT.to_string(),
            scripts,
        )
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

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, String> {
        let result = self
            .scripts
            .run(self.script.clone(), ctx.clone(), args)
            .await?;
        Ok(ToolOutput::json(result))
    }
}
