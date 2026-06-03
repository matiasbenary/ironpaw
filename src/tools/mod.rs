pub mod ai_inference;
pub mod image_generation;
pub mod inspect_file;
pub mod send_message;
pub mod todo;

use std::sync::Arc;

use async_trait::async_trait;

use crate::{history::History, openai::{FunctionTool, Tool}};

use self::{
    ai_inference::AiInferenceTool, image_generation::ImageGenerationTool,
    inspect_file::InspectFileTool, send_message::SendMessageTool, todo::TodoTool,
};

pub struct ToolOutput {
    pub result: serde_json::Value,
}

impl ToolOutput {
    pub fn json(result: serde_json::Value) -> Self {
        Self { result }
    }
}

#[derive(Clone)]
pub struct InternalTools {
    pub ai_inference: AiInferenceTool,
    pub send_message: Arc<SendMessageTool>,
    pub history: Arc<dyn History>,
    pub tools: Vec<Arc<dyn InternalTool>>,
}

impl InternalTools {
    pub fn new(
        ai_inference: AiInferenceTool,
        send_message: SendMessageTool,
        near_api_key: String,
        files_dir: std::path::PathBuf,
        history: Arc<dyn History>,
    ) -> Self {
        let send_message = Arc::new(send_message);
        let image_generation = Arc::new(ImageGenerationTool::new(near_api_key, files_dir.clone()));
        let inspect_file = Arc::new(InspectFileTool::new(files_dir, ai_inference.clone()));
        let todo = Arc::new(TodoTool::new());
        Self {
            ai_inference,
            send_message: Arc::clone(&send_message),
            history,
            tools: vec![send_message, image_generation, inspect_file, todo],
        }
    }
}

#[async_trait]
pub trait InternalTool: Send + Sync {
    fn name(&self) -> &'static str;

    fn responses_schema(&self) -> Tool;

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput, String>;
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

pub fn responses_tool_schema<P>(name: &str, description: &str) -> Tool
where
    P: schemars::JsonSchema,
{
    let mut parameters =
        serde_json::to_value(schemars::schema_for!(P)).expect("tool schema should serialize");
    sanitize_schema(&mut parameters);

    Tool::Function(FunctionTool {
        name: name.to_string(),
        description: Some(description.to_string()),
        parameters: Some(parameters),
        strict: None,
        defer_loading: None,
    })
}

pub fn schema_for_model(tool: &dyn InternalTool) -> Tool {
    let mut schema = tool.responses_schema();
    if let Tool::Function(function) = &mut schema {
        let parameters = function
            .parameters
            .get_or_insert_with(|| serde_json::json!({ "type": "object" }));
        if let Some(root) = parameters.as_object_mut() {
            root.insert("type".to_string(), "object".into());
        }
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
