use std::{
    path::PathBuf,
    sync::{Arc, OnceLock},
};

use async_trait::async_trait;

use crate::{
    message::ToolDefinition,
    scripts::ScriptRuntime,
    tools::{
        self, InternalTool, InternalTools, ToolContext, ToolOutput,
        dynamic::{DynamicTool, DynamicToolManifest, valid_tool_name},
    },
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
    /// and the value of the script's last expression becomes the tool's
    /// result. If the tool is meant to notify the user, the script must call
    /// send_message itself, with explicit channel and conversation_id.
    /// Example: `const r = tool("web_fetch", { url: "https://example.com" });
    /// r.body.length`
    pub script: String,
}

/// Lets the model author new tools as sandboxed scripts. Bound to the tool
/// registry and the script runtime after construction.
#[derive(Default)]
pub struct CreateToolTool {
    registry: OnceLock<(Arc<InternalTools>, Arc<ScriptRuntime>, PathBuf)>,
}

impl CreateToolTool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bind(&self, tools: Arc<InternalTools>, scripts: Arc<ScriptRuntime>, tools_dir: PathBuf) {
        let _ = self.registry.set((tools, scripts, tools_dir));
    }

    pub async fn execute(&self, params: CreateToolParams) -> Result<serde_json::Value, String> {
        let (tools, scripts, tools_dir) = self
            .registry
            .get()
            .ok_or_else(|| "the tool registry is not available".to_string())?;
        let name = params.name.trim().to_string();
        if !valid_tool_name(&name) {
            return Err("name must be non-empty snake_case (letters, digits, '_')".to_string());
        }
        if tools.contains(&name) {
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
        DynamicTool::write_files(tools_dir, &name, &params.script, &manifest)?;
        tools.add(Arc::new(DynamicTool::new(
            name.clone(),
            params.description,
            None,
            params.script,
            Arc::clone(scripts),
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

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput, String> {
        tools::execute_typed(args, |params: CreateToolParams| async {
            CreateToolTool::execute(self, params).await
        })
        .await
        .map(ToolOutput::json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bus::Bus,
        history::SqliteHistory,
        tools::{InternalTools, raw_inference::RawInferenceTool, send_message::SendMessageTool},
    };

    fn bound_create_tool() -> (Arc<InternalTools>, Arc<ScriptRuntime>, PathBuf) {
        let bus = Bus::new(4);
        let files_dir = std::env::temp_dir().join(format!(
            "warden-create-tool-test-files-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&files_dir).unwrap();
        let history = std::env::temp_dir().join(format!(
            "warden-create-tool-test-history-{}.db",
            uuid::Uuid::new_v4()
        ));
        let tools = Arc::new(InternalTools::new(
            RawInferenceTool::new().unwrap(),
            SendMessageTool::new(bus, |_| Err("no attachments".to_string())),
            files_dir,
            Arc::new(SqliteHistory::new(history).unwrap()),
        ));
        let scripts = Arc::new(ScriptRuntime::new(Arc::clone(&tools)));
        let tools_dir = std::env::temp_dir().join(format!(
            "warden-create-tool-test-tools-{}",
            uuid::Uuid::new_v4()
        ));
        tools
            .create_tool
            .bind(Arc::clone(&tools), Arc::clone(&scripts), tools_dir.clone());
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
                script: "({ greeting: 'hello' })".to_string(),
            })
            .await
            .expect("tool should be created");

        assert_eq!(result["ok"], true);
        assert!(tools.contains("hello_dynamic"));
        assert_eq!(
            std::fs::read_to_string(tools_dir.join("hello_dynamic.js")).unwrap(),
            "({ greeting: 'hello' })"
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
        assert!(!tools.contains("bad_script"));
        assert!(!tools_dir.join("bad_script.js").exists());
        assert!(!tools_dir.join("bad_script.config.json").exists());
    }
}
