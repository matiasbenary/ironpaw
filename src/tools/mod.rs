mod built_in;
mod context;
pub mod dynamic;
mod handler;
mod runner;
pub mod state;
pub mod wasm;

use std::{
    collections::HashSet,
    sync::{Arc, RwLock},
};

use async_trait::async_trait;

use crate::{
    bus::Bus, config::RuntimeLayout, history::History, message::ToolDefinition,
    scripts::ScriptRuntime, secrets::Secrets,
};

pub use crate::bus::{ExecutionContext, InvocationScope};
pub use built_in::{
    automation, browser, download_file, image_generation, inspect_file, messages, raw_inference,
    request_user_input, search_history, setup, todo, unlock, web,
};
pub use context::ToolResponder;
pub use handler::ToolHandler;
pub(crate) use runner::run_registered_tool_for_context;
pub use runner::{run_internal_tool, run_registered_tool, run_tool};

use self::built_in::{
    automation::{AutomationTool, AutomationToolKind},
    browser::{BrowserTool, BrowserUpdateTool},
    download_file::DownloadFileTool,
    files::{CreateFileTool, ListFilesTool, ReadFileTool, UpdateFileTool},
    image_generation::ImageGenerationTool,
    inspect_file::InspectFileTool,
    messages::{DeleteMessageTool, SendMessageTool},
    raw_inference::RawInferenceTool,
    request_user_input::RequestUserInputTool,
    search_history::SearchHistoryTool,
    setup::ListChannelsTool,
    todo::TodoTool,
    web::WebFetchTool,
};
use self::dynamic::RegisterToolTool;

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct ToolError {
    pub message: String,
}

impl ToolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl From<String> for ToolError {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

pub type ToolResult = Result<serde_json::Value, ToolError>;

pub fn tool_result_json(result: &ToolResult) -> serde_json::Value {
    match result {
        Ok(value) => serde_json::json!({ "status": "succeeded", "result": value }),
        Err(error) => serde_json::json!({ "status": "failed", "error": error.message }),
    }
}

pub struct InternalTools {
    pub raw_inference: RawInferenceTool,
    pub register_tool: Arc<RegisterToolTool>,
    pub scripts: Arc<ScriptRuntime>,
    pub history: Arc<dyn History>,
    pub registry: Arc<ToolRegistry>,
}

pub struct ToolRegistry {
    tools: RwLock<Vec<Arc<dyn InternalTool>>>,
    dynamic_names: RwLock<HashSet<String>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: RwLock::new(Vec::new()),
            dynamic_names: RwLock::new(HashSet::new()),
        }
    }

    /// Snapshot of every registered tool (internal and dynamic).
    pub fn all(&self) -> Vec<Arc<dyn InternalTool>> {
        self.tools.read().expect("tools lock").clone()
    }

    pub fn named_tools(&self, names: &[String]) -> Result<Vec<Arc<dyn InternalTool>>, String> {
        let tools = self.all();
        names
            .iter()
            .map(|name| {
                tools
                    .iter()
                    .find(|tool| tool.name() == name)
                    .cloned()
                    .ok_or_else(|| format!("unknown tool: {name}"))
            })
            .collect()
    }

    /// Register a tool at runtime. Names must be unique.
    pub fn add(&self, tool: Arc<dyn InternalTool>) -> Result<(), String> {
        let mut tools = self.tools.write().expect("tools lock");
        if tools.iter().any(|existing| existing.name() == tool.name()) {
            return Err(format!("a tool named '{}' already exists", tool.name()));
        }
        tools.push(tool);
        drop(tools);
        Ok(())
    }

    /// Register a dynamic tool, replacing only an earlier dynamic version.
    pub fn register_dynamic(&self, tool: Arc<dyn InternalTool>) -> Result<(), String> {
        let name = tool.name().to_string();
        let mut tools = self.tools.write().expect("tools lock");
        let mut dynamic_names = self.dynamic_names.write().expect("dynamic tools lock");
        if let Some(index) = tools.iter().position(|existing| existing.name() == name) {
            if !dynamic_names.contains(&name) {
                return Err(format!("cannot replace built-in tool '{name}'"));
            }
            tools[index] = tool;
        } else {
            tools.push(tool);
            dynamic_names.insert(name);
        }
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

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl InternalTools {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        bus: Bus,
        layout: RuntimeLayout,
        raw_inference: RawInferenceTool,
        send_message: SendMessageTool,
        delete_message: DeleteMessageTool,
        files_dir: std::path::PathBuf,
        history: Arc<dyn History>,
        secrets: Secrets,
    ) -> Self {
        let send_message = Arc::new(send_message);
        let delete_message = Arc::new(delete_message);
        let registry = Arc::new(ToolRegistry::new());
        let scripts = Arc::new(ScriptRuntime::new(Arc::clone(&registry)));
        let register_tool = Arc::new(RegisterToolTool::new(
            Arc::clone(&registry),
            Arc::clone(&scripts),
            layout.tools_dir(),
        ));
        fn tool<T: InternalTool + 'static>(tool: T) -> Arc<dyn InternalTool> {
            Arc::new(tool)
        }
        for tool in vec![
            tool(raw_inference.clone()),
            Arc::clone(&send_message) as Arc<dyn InternalTool>,
            Arc::clone(&delete_message) as Arc<dyn InternalTool>,
            tool(RequestUserInputTool::new(bus.clone())),
            tool(ImageGenerationTool::new(
                files_dir.clone(),
                secrets.tools.clone(),
            )),
            tool(InspectFileTool::new(
                vec![files_dir, layout.download_dir()],
                raw_inference.clone(),
            )),
            tool(DownloadFileTool::new(layout.download_dir())),
            tool(ListFilesTool::new(layout.files_dir())),
            tool(ReadFileTool::new(layout.files_dir())),
            tool(CreateFileTool::new(layout.files_dir())),
            tool(UpdateFileTool::new(layout.files_dir())),
            tool(TodoTool::new()),
            tool(AutomationTool::new(bus.clone(), AutomationToolKind::Create)),
            tool(AutomationTool::new(bus.clone(), AutomationToolKind::Edit)),
            tool(AutomationTool::new(bus.clone(), AutomationToolKind::Delete)),
            tool(AutomationTool::new(
                bus.clone(),
                AutomationToolKind::Inspect,
            )),
            tool(AutomationTool::new(bus.clone(), AutomationToolKind::List)),
            tool(AutomationTool::new(bus, AutomationToolKind::Trigger)),
            tool(ListChannelsTool::new(layout.clone(), secrets)),
            Arc::clone(&register_tool) as Arc<dyn InternalTool>,
            tool(SearchHistoryTool::new(Arc::clone(&history))),
            tool(BrowserTool::new(layout.clone())),
            tool(BrowserUpdateTool::new(layout.clone())),
            tool(WebFetchTool::new()),
        ] {
            registry.add(tool).expect("unique built-in tool name");
        }
        Self {
            raw_inference,
            register_tool: Arc::clone(&register_tool),
            scripts,
            history,
            registry,
        }
    }
}

#[async_trait]
pub trait InternalTool: Send + Sync {
    fn name(&self) -> &str;

    fn tool_definition(&self) -> ToolDefinition;

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ExecutionContext,
        responder: ToolResponder,
    );
}

pub async fn execute_typed<P, O, F>(
    args: serde_json::Value,
    execute: impl FnOnce(P) -> F,
) -> ToolResult
where
    P: serde::de::DeserializeOwned,
    O: serde::Serialize,
    F: std::future::Future<Output = Result<O, String>>,
{
    let params = serde_json::from_value(args).map_err(|err| ToolError::new(err.to_string()))?;
    let output = execute(params).await.map_err(ToolError::from)?;
    serde_json::to_value(output).map_err(|err| ToolError::new(err.to_string()))
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

            if let Some(serde_json::Value::Array(types)) = object.get_mut("type")
                && types.iter().any(|value| value.as_str() == Some("null"))
            {
                let non_null = types
                    .iter()
                    .find(|value| value.as_str() != Some("null"))
                    .cloned();
                if let Some(non_null) = non_null {
                    object.insert("type".to_string(), non_null);
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
        .and_then(serde_json::Value::as_bool)
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
    use crate::bus::{ConversationKey, Event};
    use crate::history::{History, SqliteHistory};
    use crate::message::{Message, ToolCall};

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
            ctx: &ExecutionContext,
            responder: ToolResponder,
        ) {
            responder.respond(Ok(serde_json::json!({ "depth": ctx.depth })));
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
            _ctx: &ExecutionContext,
            responder: ToolResponder,
        ) {
            responder.respond(Ok(serde_json::json!({})));
        }
    }

    fn test_tools() -> InternalTools {
        let bus = crate::bus::Bus::new(4);
        let layout = crate::config::RuntimeLayout::from_root(std::env::temp_dir().join(format!(
            "ironpaw-tools-test-runtime-{}",
            uuid::Uuid::new_v4()
        )));
        let files_dir = layout.files_dir();
        std::fs::create_dir_all(&files_dir).unwrap();
        let history = std::env::temp_dir().join(format!(
            "ironpaw-tools-test-history-{}.db",
            uuid::Uuid::new_v4()
        ));
        let secrets = crate::secrets::Secrets::for_tests();
        InternalTools::new(
            bus.clone(),
            layout,
            RawInferenceTool::new(secrets.tools.clone()).unwrap(),
            SendMessageTool::new(bus.clone(), |_| Err("no attachments".to_string())),
            DeleteMessageTool::new(bus),
            files_dir,
            Arc::new(crate::history::SqliteHistory::new(history).unwrap()),
            secrets,
        )
    }

    fn temp_history(prefix: &str) -> Arc<SqliteHistory> {
        Arc::new(
            SqliteHistory::new(
                std::env::temp_dir().join(format!("{prefix}-{}.db", uuid::Uuid::new_v4())),
            )
            .unwrap(),
        )
    }

    fn test_conversation() -> ConversationKey {
        ConversationKey {
            channel: "tui".to_string(),
            chat_id: "chat-1".to_string(),
            thread_id: None,
        }
    }

    #[test]
    fn duplicate_tool_names_are_rejected() {
        let tools = test_tools();
        tools
            .registry
            .add(Arc::new(FakeTool("fake")))
            .expect("first add");
        assert!(tools.registry.add(Arc::new(FakeTool("fake"))).is_err());
        assert!(
            tools
                .registry
                .add(Arc::new(FakeTool("send_message")))
                .is_err()
        );
    }

    #[test]
    fn validate_args_rejects_unknown_tools() {
        let tools = test_tools();
        let result = tools
            .registry
            .validate_args_for_tool("not_a_tool", &serde_json::json!({}));
        assert!(result.unwrap_err().contains("unknown tool"));
    }

    #[test]
    fn validate_args_checks_required_and_extra_fields() {
        let tools = test_tools();
        tools
            .registry
            .add(Arc::new(SchemaTool))
            .expect("add schema tool");

        assert!(
            tools
                .registry
                .validate_args_for_tool("schema_tool", &serde_json::json!({}))
                .unwrap_err()
                .contains("missing required field")
        );
        assert!(
            tools
                .registry
                .validate_args_for_tool(
                    "schema_tool",
                    &serde_json::json!({ "message": "hi", "extra": true })
                )
                .unwrap_err()
                .contains("unknown field")
        );
        assert!(
            tools
                .registry
                .validate_args_for_tool("schema_tool", &serde_json::json!({ "message": "hi" }))
                .is_ok()
        );
    }

    #[tokio::test]
    async fn run_tool_caps_call_depth() {
        let tools = test_tools();
        tools.registry.add(Arc::new(FakeTool("fake"))).expect("add");

        let mut deep = ExecutionContext::new(InvocationScope::ScheduledAutomation {
            automation_id: "test".to_string(),
        });
        deep.depth = runner::MAX_TOOL_DEPTH;
        let result = run_tool(&tools, "fake", serde_json::json!({}), &deep).await;
        assert!(result.is_err());

        let ok = run_tool(
            &tools,
            "fake",
            serde_json::json!({}),
            &ExecutionContext::new(InvocationScope::ScheduledAutomation {
                automation_id: "test".to_string(),
            }),
        )
        .await;
        assert_eq!(ok.unwrap()["depth"], 1);
    }

    #[tokio::test]
    async fn run_tool_enforces_agent_call_budget() {
        let tools = test_tools();
        tools.registry.add(Arc::new(FakeTool("fake"))).expect("add");
        let ctx = ExecutionContext::new(InvocationScope::ScheduledAutomation {
            automation_id: "test".to_string(),
        });
        ctx.tool_budget.lock().unwrap().used = runner::MAX_TOOL_CALLS;

        let result = run_tool(&tools, "fake", serde_json::json!({}), &ctx).await;

        assert!(result.unwrap_err().message.contains("limit reached"));
    }

    #[tokio::test]
    async fn tool_handler_runs_tool_requests() {
        let bus = Bus::new(16);
        let registry = Arc::new(ToolRegistry::new());
        registry.add(Arc::new(FakeTool("fake"))).expect("add");
        let history = temp_history("ironpaw-tool-handler-test");
        let handler = Arc::new(ToolHandler::new(bus.clone(), registry, history));
        let task = handler.start();
        let mut events = bus.subscribe();

        bus.publish(Event::ToolRequest {
            context: ExecutionContext::new(InvocationScope::ScheduledAutomation {
                automation_id: "test".to_string(),
            }),
            tool_call: ToolCall {
                id: "call-1".to_string(),
                name: "fake".to_string(),
                arguments: "{}".to_string(),
            },
            args: serde_json::json!({}),
            record_in_history: true,
        });

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if let Event::ToolResult {
                    tool_call_id,
                    tool_name,
                    result,
                    ..
                } = events.recv().await.expect("event")
                {
                    break (tool_call_id, tool_name, result);
                }
            }
        })
        .await
        .expect("tool result");
        task.abort();

        assert_eq!(result.0, "call-1");
        assert_eq!(result.1, "fake");
        assert_eq!(result.2.unwrap()["depth"], 1);
    }

    #[tokio::test]
    async fn tool_handler_persists_automation_tool_transcript() {
        let bus = Bus::new(16);
        let registry = Arc::new(ToolRegistry::new());
        registry.add(Arc::new(FakeTool("fake"))).expect("add");
        let history = temp_history("ironpaw-tool-handler-automation-history");
        let handler = Arc::new(ToolHandler::new(bus.clone(), registry, history.clone()));
        let task = handler.start();
        let mut events = bus.subscribe();
        let automation_id = "daily-album";
        let ctx = ExecutionContext::new(InvocationScope::ScheduledAutomation {
            automation_id: automation_id.to_string(),
        });

        bus.publish(Event::ToolRequest {
            context: ctx,
            tool_call: ToolCall {
                id: "call-1".to_string(),
                name: "fake".to_string(),
                arguments: "{}".to_string(),
            },
            args: serde_json::json!({}),
            record_in_history: true,
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if matches!(
                    events.recv().await.expect("event"),
                    Event::ToolResult { .. }
                ) {
                    break;
                }
            }
        })
        .await
        .expect("tool result");
        task.abort();

        let scope = crate::history::HistoryScope::Automation(automation_id.to_string());
        let entries = history.load_scope(&scope).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(
            entries
                .iter()
                .all(|entry| entry.metadata.automation_id.as_deref() == Some(automation_id))
        );
        assert!(matches!(entries[0].message, Message::Assistant { .. }));
        assert!(matches!(entries[1].message, Message::ToolResult { .. }));
    }

    #[tokio::test]
    async fn tool_handler_persists_bus_tool_transcript() {
        let bus = Bus::new(16);
        let registry = Arc::new(ToolRegistry::new());
        registry.add(Arc::new(FakeTool("fake"))).expect("add");
        let history = temp_history("ironpaw-tool-handler-bus-history");
        let handler = Arc::new(ToolHandler::new(bus.clone(), registry, history.clone()));
        let task = handler.start();
        let mut events = bus.subscribe();
        let conversation = test_conversation();

        bus.publish(Event::ToolRequest {
            context: ExecutionContext::new(InvocationScope::Conversation(conversation.clone())),
            tool_call: ToolCall {
                id: "call-1".to_string(),
                name: "fake".to_string(),
                arguments: "{}".to_string(),
            },
            args: serde_json::json!({}),
            record_in_history: true,
        });

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if matches!(
                    events.recv().await.expect("event"),
                    Event::ToolResult { .. }
                ) {
                    break;
                }
            }
        })
        .await
        .expect("tool result");
        task.abort();

        let entries = history.load(&conversation).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(matches!(entries[0].message, Message::Assistant { .. }));
        assert!(matches!(entries[1].message, Message::ToolResult { .. }));
    }
}
