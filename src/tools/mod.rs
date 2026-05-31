pub mod ai_inference;
pub mod image_generation;
pub mod read_file;
pub mod send_message;

use std::sync::Arc;

use async_trait::async_trait;

use crate::message::OutboundAttachment;
use crate::openai::{FunctionCallOutput, FunctionTool, Tool};

use self::
    {ai_inference::AiInferenceTool, image_generation::ImageGenerationTool,
    read_file::ReadFileTool, send_message::SendMessageTool};

pub struct ToolOutput {
    pub model_output: FunctionCallOutput,
    pub files: Vec<OutboundAttachment>,
}

impl ToolOutput {
    pub fn json(result: serde_json::Value) -> Self {
        Self {
            model_output: FunctionCallOutput::Text(result.to_string()),
            files: Vec::new(),
        }
    }
}

#[derive(Clone)]
pub struct InternalTools {
    pub ai_inference: AiInferenceTool,
    pub send_message: Arc<SendMessageTool>,
    pub tools: Vec<Arc<dyn InternalTool>>,
}

impl InternalTools {
    pub fn new(
        ai_inference: AiInferenceTool,
        send_message: SendMessageTool,
        near_api_key: String,
        files_dir: std::path::PathBuf,
    ) -> Self {
        let send_message = Arc::new(send_message);
        let image_generation = Arc::new(ImageGenerationTool::new(near_api_key, files_dir.clone()));
        let read_file = Arc::new(ReadFileTool::new(files_dir));
        Self {
            ai_inference,
            send_message: Arc::clone(&send_message),
            tools: vec![send_message, image_generation, read_file],
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
    Tool::Function(FunctionTool {
        name: name.to_string(),
        description: Some(description.to_string()),
        parameters: Some(
            serde_json::to_value(schemars::schema_for!(P)).expect("tool schema should serialize"),
        ),
        strict: None,
        defer_loading: None,
    })
}
