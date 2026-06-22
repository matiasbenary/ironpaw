pub mod call_agent;
pub mod continue_conversation;
pub mod create_automation;
pub mod create_tool;
pub mod delete_automation;
pub mod delete_message;
pub mod dynamic;
pub mod image_generation;
pub mod inspect_automation;
pub mod inspect_file;
pub mod install_channel;
pub mod list_automations;
pub mod onboarding;
pub mod raw_inference;
pub mod search_history;
pub mod send_message;
pub mod todo;
pub mod web_fetch;
pub mod web_search;

use std::sync::{Arc, RwLock};

use async_trait::async_trait;

use crate::{bus::ChannelAddress, history::History, message::ToolDefinition};

use self::{
    call_agent::CallAgentTool,
    create_automation::CreateAutomationTool,
    create_tool::CreateToolTool,
    delete_automation::DeleteAutomationTool,
    delete_message::DeleteMessageTool,
    image_generation::ImageGenerationTool,
    inspect_automation::InspectAutomationTool,
    inspect_file::InspectFileTool,
    install_channel::{InstallChannelTool, ListChannelsTool},
    list_automations::ListAutomationsTool,
    onboarding::OnboardingTool,
    raw_inference::RawInferenceTool,
    search_history::SearchHistoryTool,
    send_message::SendMessageTool,
    todo::TodoTool,
    web_fetch::WebFetchTool,
    web_search::WebSearchTool,
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
    pub delete_message: Arc<DeleteMessageTool>,
    pub call_agent: Arc<CallAgentTool>,
    pub create_automation: Arc<CreateAutomationTool>,
    pub delete_automation: Arc<DeleteAutomationTool>,
    pub inspect_automation: Arc<InspectAutomationTool>,
    pub list_automations: Arc<ListAutomationsTool>,
    pub list_channels: Arc<ListChannelsTool>,
    pub onboarding: Arc<OnboardingTool>,
    pub install_channel: Arc<InstallChannelTool>,
    pub create_tool: Arc<CreateToolTool>,
    pub history: Arc<dyn History>,
    tools: RwLock<Vec<Arc<dyn InternalTool>>>,
}

impl InternalTools {
    pub fn new(
        raw_inference: RawInferenceTool,
        send_message: SendMessageTool,
        delete_message: DeleteMessageTool,
        files_dir: std::path::PathBuf,
        history: Arc<dyn History>,
    ) -> Self {
        send_message.bind_history(Arc::clone(&history));
        delete_message.bind_history(Arc::clone(&history));
        let send_message = Arc::new(send_message);
        let delete_message = Arc::new(delete_message);
        let image_generation = Arc::new(ImageGenerationTool::new(files_dir.clone()));
        let inspect_file = Arc::new(InspectFileTool::new(files_dir, raw_inference.clone()));
        let todo = Arc::new(TodoTool::new());
        let call_agent = Arc::new(CallAgentTool::new());
        let create_automation = Arc::new(CreateAutomationTool::new());
        let delete_automation = Arc::new(DeleteAutomationTool::new());
        let inspect_automation = Arc::new(InspectAutomationTool::new());
        let list_automations = Arc::new(ListAutomationsTool::new());
        let list_channels = Arc::new(ListChannelsTool::new());
        let onboarding = Arc::new(OnboardingTool::new());
        let install_channel = Arc::new(InstallChannelTool::new());
        let create_tool = Arc::new(CreateToolTool::new());
        let web_fetch = Arc::new(WebFetchTool::new());
        let web_search = Arc::new(WebSearchTool::new());
        let search_history = Arc::new(SearchHistoryTool::new(Arc::clone(&history)));
        Self {
            raw_inference: raw_inference.clone(),
            send_message: Arc::clone(&send_message),
            delete_message: Arc::clone(&delete_message),
            call_agent: Arc::clone(&call_agent),
            create_automation: Arc::clone(&create_automation),
            delete_automation: Arc::clone(&delete_automation),
            inspect_automation: Arc::clone(&inspect_automation),
            list_automations: Arc::clone(&list_automations),
            list_channels: Arc::clone(&list_channels),
            onboarding: Arc::clone(&onboarding),
            install_channel: Arc::clone(&install_channel),
            create_tool: Arc::clone(&create_tool),
            history,
            tools: RwLock::new(vec![
                Arc::new(raw_inference),
                send_message,
                delete_message,
                image_generation,
                inspect_file,
                todo,
                call_agent,
                create_automation,
                delete_automation,
                inspect_automation,
                list_automations,
                list_channels,
                onboarding,
                install_channel,
                create_tool,
                search_history,
                web_fetch,
                web_search,
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

    pub fn validate_args_for_tool(
        &self,
        name: &str,
        args: &serde_json::Value,
    ) -> Result<(), String> {
        let tool = self
            .all()
            .into_iter()
            .find(|tool| tool.name() == name)
            .ok_or_else(|| format!("unknown tool: {name}"))?;
        validate_json_schema(args, &tool.tool_definition().parameters)
            .map_err(|err| format!("invalid args for tool '{name}': {err}"))
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
    let tool = tools.all().iter().find(|tool| tool.name() == name).cloned();
    let Some(tool) = tool else {
        return serde_json::json!({ "ok": false, "error": format!("unknown tool: {name}") });
    };
    run_internal_tool(tool, args, ctx).await
}

pub async fn run_internal_tool(
    tool: Arc<dyn InternalTool>,
    args: serde_json::Value,
    ctx: &ToolContext,
) -> serde_json::Value {
    if ctx.depth >= MAX_TOOL_DEPTH {
        return serde_json::json!({
            "ok": false,
            "error": format!("tool call chain exceeded max depth ({MAX_TOOL_DEPTH})"),
        });
    }
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

fn validate_json_schema(
    value: &serde_json::Value,
    schema: &serde_json::Value,
) -> Result<(), String> {
    if let Some(types) = schema.get("type").and_then(|value| value.as_array()) {
        if types
            .iter()
            .any(|schema_type| validate_type(value, schema_type.as_str().unwrap_or("")).is_ok())
        {
            return Ok(());
        }
        return Err(format!("expected one of {types:?}"));
    }

    if let Some(schema_type) = schema.get("type").and_then(|value| value.as_str()) {
        validate_type(value, schema_type)?;
    }

    match value {
        serde_json::Value::Object(object) => validate_object(object, schema),
        serde_json::Value::Array(items) => {
            if let Some(item_schema) = schema.get("items") {
                for item in items {
                    validate_json_schema(item, item_schema)?;
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn validate_type(value: &serde_json::Value, schema_type: &str) -> Result<(), String> {
    let valid = match schema_type {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => value.is_number(),
        "null" => value.is_null(),
        _ => true,
    };
    if valid {
        Ok(())
    } else {
        Err(format!("expected {schema_type}"))
    }
}

fn validate_object(
    object: &serde_json::Map<String, serde_json::Value>,
    schema: &serde_json::Value,
) -> Result<(), String> {
    if let Some(required) = schema.get("required").and_then(|value| value.as_array()) {
        for field in required.iter().filter_map(|field| field.as_str()) {
            if !object.contains_key(field) {
                return Err(format!("missing required field '{field}'"));
            }
        }
    }

    let properties = schema.get("properties").and_then(|value| value.as_object());
    if schema
        .get("additionalProperties")
        .and_then(|value| value.as_bool())
        == Some(false)
    {
        for key in object.keys() {
            if properties.is_none_or(|properties| !properties.contains_key(key)) {
                return Err(format!("unknown field '{key}'"));
            }
        }
    }

    if let Some(properties) = properties {
        for (key, value) in object {
            if let Some(property_schema) = properties.get(key) {
                validate_json_schema(value, property_schema)
                    .map_err(|err| format!("{key}: {err}"))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeTool(&'static str);

    struct SchemaTool;

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

    #[async_trait]
    impl InternalTool for SchemaTool {
        fn name(&self) -> &str {
            "schema_tool"
        }

        fn tool_definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: self.name().to_string(),
                description: String::new(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "message": { "type": "string" }
                    },
                    "required": ["message"],
                    "additionalProperties": false
                }),
            }
        }

        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &ToolContext,
        ) -> Result<ToolOutput, String> {
            Ok(ToolOutput::json(serde_json::json!({})))
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
            RawInferenceTool::new().unwrap(),
            SendMessageTool::new(bus.clone(), |_| Err("no attachments".to_string())),
            DeleteMessageTool::new(bus),
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

    #[test]
    fn validate_args_rejects_unknown_tools() {
        let tools = test_tools();
        let result = tools.validate_args_for_tool("not_a_tool", &serde_json::json!({}));
        assert!(result.unwrap_err().contains("unknown tool"));
    }

    #[test]
    fn validate_args_checks_required_and_extra_fields() {
        let tools = test_tools();
        tools.add(Arc::new(SchemaTool)).expect("add schema tool");

        assert!(
            tools
                .validate_args_for_tool("schema_tool", &serde_json::json!({}))
                .unwrap_err()
                .contains("missing required field")
        );
        assert!(
            tools
                .validate_args_for_tool(
                    "schema_tool",
                    &serde_json::json!({ "message": "hi", "extra": true })
                )
                .unwrap_err()
                .contains("unknown field")
        );
        assert!(
            tools
                .validate_args_for_tool("schema_tool", &serde_json::json!({ "message": "hi" }))
                .is_ok()
        );
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
