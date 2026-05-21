pub mod ai_inference;
pub mod send_message;

use std::sync::Arc;

use async_trait::async_trait;

use crate::openai::{ChatCompletionTool, ChatCompletionTools, FunctionObject};

use self::{ai_inference::AiInferenceTool, send_message::SendMessageTool};

#[derive(Clone)]
pub struct InternalTools {
    pub ai_inference: AiInferenceTool,
    pub send_message: Arc<SendMessageTool>,
    pub tools: Vec<Arc<dyn InternalTool>>,
}

impl InternalTools {
    pub fn new(ai_inference: AiInferenceTool, send_message: SendMessageTool) -> Self {
        let ai_inference_tool = Arc::new(ai_inference.clone());
        let send_message = Arc::new(send_message);
        Self {
            ai_inference,
            send_message: Arc::clone(&send_message),
            tools: vec![ai_inference_tool, send_message],
        }
    }
}

#[async_trait]
pub trait InternalTool: Send + Sync {
    fn name(&self) -> &'static str;

    fn openai_schema(&self) -> ChatCompletionTools;

    async fn execute(&self, args: serde_json::Value) -> Result<serde_json::Value, String>;
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

pub fn openai_tool_schema<P>(name: &str, description: &str) -> ChatCompletionTools
where
    P: schemars::JsonSchema,
{
    ChatCompletionTools::Function(ChatCompletionTool {
        function: FunctionObject {
            name: name.to_string(),
            description: Some(description.to_string()),
            parameters: Some(
                serde_json::to_value(schemars::schema_for!(P))
                    .expect("tool schema should serialize"),
            ),
            strict: None,
        },
    })
}
