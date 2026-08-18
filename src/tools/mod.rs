macro_rules! tool_metadata {
    ($name:literal, $params:ty, $description:expr) => {
        fn name(&self) -> &str {
            $name
        }
        fn tool_definition(&self) -> $crate::message::ToolDefinition {
            $crate::tools::tool_definition::<$params>($name, $description)
        }
    };
}
pub(crate) use tool_metadata;

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
    bus::Bus, config::RuntimeLayout, history::History, message::ToolDefinition, secrets::Secrets,
    shell_tools::ShellExecutor,
};

pub use built_in::{
    automation, download_file, image_generation, inference, inspect_file, messages,
    request_user_input, search_history, shell, todo, unlock, web,
};
pub use context::ExecutionContext;
pub use handler::ToolHandler;
#[cfg(test)]
pub(crate) use runner::execute_resolved_tool;
pub use runner::execute_tool;

use self::built_in::{
    automation::{AutomationTool, AutomationToolKind},
    download_file::DownloadFileTool,
    files::{CreateFileTool, ListFilesTool, ReadFileTool, UpdateFileTool},
    image_generation::ImageGenerationTool,
    inference::RawInferenceTool,
    inspect_file::InspectFileTool,
    messages::{DeleteMessageTool, SendMessageTool},
    request_user_input::RequestUserInputTool,
    search_history::SearchHistoryTool,
    shell::{ListShellCommandsTool, ShellTool},
    todo::TodoTool,
    web::WebFetchTool,
};
use self::dynamic::{DynamicToolKind, DynamicToolTool, JavaScriptRuntime};
use crate::setup::{ListChannelsTool, ListToolsTool};

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

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub enum ToolOutcome<T> {
    Result(T),
    EndTurn(T),
}

impl<T> ToolOutcome<T> {
    fn into_result(self) -> T {
        match self {
            Self::Result(result) | Self::EndTurn(result) => result,
        }
    }

    pub(crate) fn map<U>(self, map: impl FnOnce(T) -> U) -> ToolOutcome<U> {
        match self {
            Self::Result(result) => ToolOutcome::Result(map(result)),
            Self::EndTurn(result) => ToolOutcome::EndTurn(map(result)),
        }
    }
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct ToolCallResult {
    pub tool_call_id: String,
    pub tool_name: String,
    pub result: ToolResult,
}

pub fn tool_result_json(result: &ToolResult) -> serde_json::Value {
    match result {
        Ok(value) => serde_json::json!({ "status": "succeeded", "result": value }),
        Err(error) => serde_json::json!({ "status": "failed", "error": error.message }),
    }
}

pub struct InternalTools {
    pub inference: crate::inference::Inference,
    pub scripts: Arc<JavaScriptRuntime>,
    pub history: Arc<dyn History>,
    pub registry: Arc<ToolRegistry>,
    pub shell_executor: ShellExecutor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolVisibility {
    Agent,
    Internal,
}

struct RegisteredTool {
    tool: Arc<dyn InternalTool>,
    visibility: ToolVisibility,
}

pub struct ToolRegistry {
    tools: RwLock<Vec<RegisteredTool>>,
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
        self.tools
            .read()
            .expect("tools lock")
            .iter()
            .map(|entry| Arc::clone(&entry.tool))
            .collect()
    }

    /// Snapshot of the tools exposed to conversation agents.
    pub fn agent_tools(&self) -> Vec<Arc<dyn InternalTool>> {
        self.tools
            .read()
            .expect("tools lock")
            .iter()
            .filter(|entry| entry.visibility == ToolVisibility::Agent)
            .map(|entry| Arc::clone(&entry.tool))
            .collect()
    }

    pub fn named_tools<S>(
        &self,
        names: impl IntoIterator<Item = S>,
    ) -> Result<Vec<Arc<dyn InternalTool>>, String>
    where
        S: AsRef<str>,
    {
        let tools = self.all();
        names
            .into_iter()
            .map(|name| {
                let name = name.as_ref();
                tools
                    .iter()
                    .find(|tool| tool.name() == name)
                    .cloned()
                    .ok_or_else(|| format!("unknown tool: {name}"))
            })
            .collect()
    }

    /// Register a tool at runtime. Names must be unique.
    pub fn add(
        &self,
        tool: Arc<dyn InternalTool>,
        visibility: ToolVisibility,
    ) -> Result<(), String> {
        let mut tools = self.tools.write().expect("tools lock");
        if tools
            .iter()
            .any(|existing| existing.tool.name() == tool.name())
        {
            return Err(format!("a tool named '{}' already exists", tool.name()));
        }
        tools.push(RegisteredTool { tool, visibility });
        Ok(())
    }

    pub fn replace(&self, tool: Arc<dyn InternalTool>) -> Result<(), String> {
        let mut tools = self.tools.write().expect("tools lock");
        let Some(existing) = tools
            .iter_mut()
            .find(|existing| existing.tool.name() == tool.name())
        else {
            return Err(format!("unknown tool: {}", tool.name()));
        };
        existing.tool = tool;
        Ok(())
    }

    /// Register a dynamic tool, replacing only an earlier dynamic version.
    pub fn register_dynamic(&self, tool: Arc<dyn InternalTool>) -> Result<(), String> {
        let name = tool.name().to_string();
        let mut tools = self.tools.write().expect("tools lock");
        let mut dynamic_names = self.dynamic_names.write().expect("dynamic tools lock");
        if let Some(index) = tools
            .iter()
            .position(|existing| existing.tool.name() == name)
        {
            if !dynamic_names.contains(&name) {
                return Err(format!("cannot replace built-in tool '{name}'"));
            }
            tools[index].tool = tool;
        } else {
            tools.push(RegisteredTool {
                tool,
                visibility: ToolVisibility::Agent,
            });
            dynamic_names.insert(name);
        }
        Ok(())
    }

    pub fn remove_dynamic(&self, name: &str) -> Result<bool, String> {
        let mut tools = self.tools.write().expect("tools lock");
        let mut dynamic_names = self.dynamic_names.write().expect("dynamic tools lock");
        if !dynamic_names.remove(name) {
            return Ok(false);
        }
        let index = tools
            .iter()
            .position(|entry| entry.tool.name() == name)
            .ok_or_else(|| format!("dynamic tool registry is missing '{name}'"))?;
        tools.remove(index);
        Ok(true)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tools
            .read()
            .expect("tools lock")
            .iter()
            .any(|entry| entry.tool.name() == name)
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
        inference: crate::inference::Inference,
        send_message: SendMessageTool,
        delete_message: DeleteMessageTool,
        files_dir: std::path::PathBuf,
        history: Arc<dyn History>,
        secrets: Secrets,
    ) -> Self {
        let send_message = Arc::new(send_message);
        let delete_message = Arc::new(delete_message);
        let registry = Arc::new(ToolRegistry::new());
        let scripts = Arc::new(JavaScriptRuntime::new(Arc::clone(&registry)));
        let shell_executor = ShellExecutor::new(layout.tools_dir(), secrets.tools.clone());
        let dynamic_tool = |kind| {
            Arc::new(DynamicToolTool::new(
                Arc::clone(&registry),
                Arc::clone(&scripts),
                layout.tools_dir(),
                kind,
            ))
        };
        let create_tool = dynamic_tool(DynamicToolKind::Create);
        let edit_tool = dynamic_tool(DynamicToolKind::Edit);
        let delete_tool = dynamic_tool(DynamicToolKind::Delete);
        fn tool<T: InternalTool + 'static>(tool: T) -> Arc<dyn InternalTool> {
            Arc::new(tool)
        }
        for tool in vec![
            tool(RawInferenceTool::new(inference.clone())),
            Arc::clone(&send_message) as Arc<dyn InternalTool>,
            Arc::clone(&delete_message) as Arc<dyn InternalTool>,
            tool(RequestUserInputTool::new(bus.clone())),
            tool(ImageGenerationTool::new(
                files_dir.clone(),
                secrets.tools.clone(),
                Arc::clone(inference.provider()),
            )),
            tool(InspectFileTool::new(files_dir, inference.clone())),
            tool(DownloadFileTool::new(layout.files_dir())),
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
            tool(ListChannelsTool::new(layout.clone(), secrets.clone())),
            tool(ListToolsTool::new(layout.clone(), secrets.clone())),
            tool(ListShellCommandsTool::new(layout.clone())),
            tool(ShellTool::new(layout.clone(), shell_executor.clone())),
            Arc::clone(&create_tool) as Arc<dyn InternalTool>,
            Arc::clone(&edit_tool) as Arc<dyn InternalTool>,
            Arc::clone(&delete_tool) as Arc<dyn InternalTool>,
            tool(SearchHistoryTool::new(Arc::clone(&history))),
            tool(WebFetchTool::new()),
        ] {
            registry
                .add(tool, ToolVisibility::Agent)
                .expect("unique built-in tool name");
        }
        Self {
            inference,
            scripts,
            history,
            registry,
            shell_executor,
        }
    }
}

#[async_trait]
pub trait InternalTool: Send + Sync {
    fn name(&self) -> &str;

    fn tool_definition(&self) -> ToolDefinition;

    async fn execute(&self, args: serde_json::Value, ctx: &ExecutionContext) -> ToolResult;

    async fn outcome(
        &self,
        args: serde_json::Value,
        ctx: &ExecutionContext,
    ) -> ToolOutcome<ToolResult> {
        ToolOutcome::Result(self.execute(args, ctx).await)
    }
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
    use crate::bus::Event;
    use crate::message::ToolCall;

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

        async fn execute(&self, _args: serde_json::Value, ctx: &ExecutionContext) -> ToolResult {
            Ok(serde_json::json!({ "depth": ctx.depth }))
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

        async fn execute(&self, _args: serde_json::Value, _ctx: &ExecutionContext) -> ToolResult {
            Ok(serde_json::json!({}))
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
            crate::inference::Inference::new(secrets.tools.clone()).unwrap(),
            SendMessageTool::new(bus.clone(), |_, _| Err("no attachments".to_string())),
            DeleteMessageTool::new(bus),
            files_dir,
            Arc::new(crate::history::SqliteHistory::new(history).unwrap()),
            secrets,
        )
    }

    #[tokio::test]
    async fn execute_tool_caps_call_depth() {
        let tools = test_tools();
        tools
            .registry
            .add(Arc::new(FakeTool("fake")), ToolVisibility::Internal)
            .expect("add");

        let mut deep = ExecutionContext::new(std::path::PathBuf::from("test"));
        deep.depth = runner::MAX_TOOL_DEPTH;
        let result = execute_tool(&tools.registry, "fake", serde_json::json!({}), &deep).await;
        assert!(result.is_err());

        let ok = execute_tool(
            &tools.registry,
            "fake",
            serde_json::json!({}),
            &ExecutionContext::new(std::path::PathBuf::from("test")),
        )
        .await;
        assert_eq!(ok.unwrap()["depth"], 1);
    }

    #[tokio::test]
    async fn execute_tool_enforces_agent_call_budget() {
        let tools = test_tools();
        tools
            .registry
            .add(Arc::new(FakeTool("fake")), ToolVisibility::Internal)
            .expect("add");
        let ctx = ExecutionContext::new(std::path::PathBuf::from("test"));
        ctx.tool_budget.lock().unwrap().used = runner::MAX_TOOL_CALLS;

        let result = execute_tool(&tools.registry, "fake", serde_json::json!({}), &ctx).await;

        assert!(result.unwrap_err().message.contains("limit reached"));
    }

    #[tokio::test]
    async fn tool_handler_returns_the_interface_for_invalid_arguments() {
        let bus = Bus::new(16);
        let registry = Arc::new(ToolRegistry::new());
        registry
            .add(Arc::new(SchemaTool), ToolVisibility::Internal)
            .expect("add");
        let handler = Arc::new(ToolHandler::new(bus.clone(), registry));
        let task = handler.start();
        let mut events = bus.subscribe();

        bus.publish(Event::ToolRequest {
            request_id: "invalid-args-request".to_string(),
            context: ExecutionContext::new(std::path::PathBuf::from("invalid-args")),
            tool_calls: vec![ToolCall {
                id: "call-1".to_string(),
                name: "schema_tool".to_string(),
                arguments: "{}".to_string(),
            }],
            allowed_tool_names: None,
        });

        let results = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if let Event::ToolResponse { outcome, .. } = events.recv().await.expect("event") {
                    break outcome.into_result();
                }
            }
        })
        .await
        .expect("tool response");
        task.abort();

        let error = &results[0].result.as_ref().unwrap_err().message;
        assert!(error.contains("missing required field"));
        assert!(error.contains("Expected parameters"));
        assert!(error.contains("\"message\""));
    }
}
