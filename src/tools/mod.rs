pub mod call_agent;
pub mod create_automation;
pub mod create_tool;
pub mod delete_automation;
pub mod dynamic;
pub mod image_generation;
pub mod inspect_file;
pub mod raw_inference;
pub mod send_message;
pub mod todo;
pub mod web_fetch;

use std::sync::{Arc, RwLock};

use async_trait::async_trait;

use crate::{bus::ChannelAddress, history::History, message::ToolDefinition};

use self::{
    call_agent::CallAgentTool, create_automation::CreateAutomationTool,
    create_tool::CreateToolTool, delete_automation::DeleteAutomationTool,
    image_generation::ImageGenerationTool, inspect_file::InspectFileTool,
    raw_inference::RawInferenceTool, send_message::SendMessageTool, todo::TodoTool,
    web_fetch::WebFetchTool,
};

/// How deep tool-calls-tool chains may go (dynamic tools and agents can
/// call tools, which can call them again).
const MAX_TOOL_DEPTH: usize = 8;

/// Context every tool receives alongside its arguments. Tools use what they
/// need and ignore the rest.
#[derive(Clone, Debug, Default)]
pub struct ToolContext {
    /// Channel + address of the event that triggered the current run, if any.
    pub destination: Option<(String, ChannelAddress)>,
    /// Conversation the current agent run belongs to.
    pub conversation_id: Option<String>,
    /// Tool call-chain depth, incremented by `run_tool`.
    pub depth: usize,
}

impl ToolContext {
    fn deeper(&self) -> Self {
        let mut ctx = self.clone();
        ctx.depth += 1;
        ctx
    }
}

pub struct ToolOutput {
    pub result: serde_json::Value,
}

impl ToolOutput {
    pub fn json(result: serde_json::Value) -> Self {
        Self { result }
    }
}

pub struct InternalTools {
    pub raw_inference: RawInferenceTool,
    pub send_message: Arc<SendMessageTool>,
    pub call_agent: Arc<CallAgentTool>,
    pub create_automation: Arc<CreateAutomationTool>,
    pub delete_automation: Arc<DeleteAutomationTool>,
    pub create_tool: Arc<CreateToolTool>,
    pub history: Arc<dyn History>,
    tools: RwLock<Vec<Arc<dyn InternalTool>>>,
}

impl InternalTools {
    pub fn new(
        raw_inference: RawInferenceTool,
        send_message: SendMessageTool,
        near_api_key: String,
        files_dir: std::path::PathBuf,
        history: Arc<dyn History>,
    ) -> Self {
        let send_message = Arc::new(send_message);
        let image_generation = Arc::new(ImageGenerationTool::new(near_api_key, files_dir.clone()));
        let inspect_file = Arc::new(InspectFileTool::new(files_dir, raw_inference.clone()));
        let todo = Arc::new(TodoTool::new());
        let call_agent = Arc::new(CallAgentTool::new());
        let create_automation = Arc::new(CreateAutomationTool::new());
        let delete_automation = Arc::new(DeleteAutomationTool::new());
        let create_tool = Arc::new(CreateToolTool::new());
        let web_fetch = Arc::new(WebFetchTool::new());
        Self {
            raw_inference: raw_inference.clone(),
            send_message: Arc::clone(&send_message),
            call_agent: Arc::clone(&call_agent),
            create_automation: Arc::clone(&create_automation),
            delete_automation: Arc::clone(&delete_automation),
            create_tool: Arc::clone(&create_tool),
            history,
            tools: RwLock::new(vec![
                Arc::new(raw_inference),
                send_message,
                image_generation,
                inspect_file,
                todo,
                call_agent,
                create_automation,
                delete_automation,
                create_tool,
                web_fetch,
            ]),
        }
    }

    /// Snapshot of every registered tool (internal and dynamic).
    pub fn all(&self) -> Vec<Arc<dyn InternalTool>> {
        self.tools.read().expect("tools lock").clone()
    }

    /// Register a tool at runtime. Names must be unique.
    pub fn add(&self, tool: Arc<dyn InternalTool>) -> Result<(), String> {
        let mut tools = self.tools.write().expect("tools lock");
        if tools.iter().any(|existing| existing.name() == tool.name()) {
            return Err(format!("a tool named '{}' already exists", tool.name()));
        }
        tools.push(tool);
        Ok(())
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tools
            .read()
            .expect("tools lock")
            .iter()
            .any(|tool| tool.name() == name)
    }
}

#[async_trait]
pub trait InternalTool: Send + Sync {
    fn name(&self) -> &str;

    fn tool_definition(&self) -> ToolDefinition;

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, String>;
}

/// Execute a tool by name, folding errors into the result JSON so every
/// caller (agent loop, scripts, automations) sees the same shape.
pub async fn run_tool(
    tools: &InternalTools,
    name: &str,
    args: serde_json::Value,
    ctx: &ToolContext,
) -> serde_json::Value {
    if ctx.depth >= MAX_TOOL_DEPTH {
        return serde_json::json!({
            "ok": false,
            "error": format!("tool call chain exceeded max depth ({MAX_TOOL_DEPTH})"),
        });
    }
    let tool = tools.all().iter().find(|tool| tool.name() == name).cloned();
    let Some(tool) = tool else {
        return serde_json::json!({ "ok": false, "error": format!("unknown tool: {name}") });
    };
    match tool.execute(args, &ctx.deeper()).await {
        Ok(ToolOutput { result }) => result,
        Err(err) => serde_json::json!({ "ok": false, "error": err }),
    }
}

pub async fn execute_typed<P, O, F>(
    args: serde_json::Value,
    execute: impl FnOnce(P) -> F,
) -> Result<serde_json::Value, String>
where
    P: serde::de::DeserializeOwned,
    O: serde::Serialize,
    F: std::future::Future<Output = Result<O, String>>,
{
    let params = serde_json::from_value(args).map_err(|err| err.to_string())?;
    let output = execute(params).await?;
    serde_json::to_value(output).map_err(|err| err.to_string())
}

pub fn tool_definition<P>(name: &str, description: &str) -> ToolDefinition
where
    P: schemars::JsonSchema,
{
    let mut parameters =
        serde_json::to_value(schemars::schema_for!(P)).expect("tool schema should serialize");
    sanitize_schema(&mut parameters);

    ToolDefinition {
        name: name.to_string(),
        description: description.to_string(),
        parameters,
    }
}

pub fn definition_for_model(tool: &dyn InternalTool) -> ToolDefinition {
    let mut schema = tool.tool_definition();
    if let Some(root) = schema.parameters.as_object_mut() {
        root.insert("type".to_string(), "object".into());
    }
    schema
}

fn sanitize_schema(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            object.remove("$schema");
            object.remove("default");
            object.remove("format");
            object.remove("title");

            if let Some(serde_json::Value::Array(types)) = object.get_mut("type") {
                if types.iter().any(|value| value.as_str() == Some("null")) {
                    let non_null = types
                        .iter()
                        .find(|value| value.as_str() != Some("null"))
                        .cloned();
                    if let Some(non_null) = non_null {
                        object.insert("type".to_string(), non_null);
                    }
                }
            }

            for value in object.values_mut() {
                sanitize_schema(value);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                sanitize_schema(value);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeTool(&'static str);

    #[async_trait]
    impl InternalTool for FakeTool {
        fn name(&self) -> &str {
            self.0
        }

        fn tool_definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: self.0.to_string(),
                description: String::new(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            ctx: &ToolContext,
        ) -> Result<ToolOutput, String> {
            Ok(ToolOutput::json(serde_json::json!({ "depth": ctx.depth })))
        }
    }

    fn test_tools() -> InternalTools {
        let bus = crate::bus::Bus::new(4);
        let files_dir = std::env::temp_dir().join("warden-tools-test-files");
        std::fs::create_dir_all(&files_dir).unwrap();
        let history = std::env::temp_dir().join(format!(
            "warden-tools-test-history-{}.db",
            uuid::Uuid::new_v4()
        ));
        InternalTools::new(
            RawInferenceTool::new("test-key".to_string()).unwrap(),
            SendMessageTool::new(bus, |_| Err("no attachments".to_string())),
            "test-key".to_string(),
            files_dir,
            Arc::new(crate::history::SqliteHistory::new(history).unwrap()),
        )
    }

    #[test]
    fn duplicate_tool_names_are_rejected() {
        let tools = test_tools();
        tools.add(Arc::new(FakeTool("fake"))).expect("first add");
        assert!(tools.add(Arc::new(FakeTool("fake"))).is_err());
        assert!(tools.add(Arc::new(FakeTool("send_message"))).is_err());
    }

    #[tokio::test]
    async fn run_tool_caps_call_depth() {
        let tools = test_tools();
        tools.add(Arc::new(FakeTool("fake"))).expect("add");

        let deep = ToolContext {
            depth: MAX_TOOL_DEPTH,
            ..ToolContext::default()
        };
        let result = run_tool(&tools, "fake", serde_json::json!({}), &deep).await;
        assert_eq!(result["ok"], false);

        let ok = run_tool(
            &tools,
            "fake",
            serde_json::json!({}),
            &ToolContext::default(),
        )
        .await;
        assert_eq!(ok["depth"], 1);
    }
}
