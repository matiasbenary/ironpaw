use async_trait::async_trait;

use crate::{
    inference::{Inference, InferenceRequest},
    tools::{self, InternalTool, ToolError, ToolResponder},
};

/// LLM-callable wrapper around one model completion with raw messages and
/// raw results. The inference implementation itself lives in `crate::inference`.
#[derive(Clone)]
pub struct RawInferenceTool {
    inference: Inference,
}

impl RawInferenceTool {
    pub fn new(inference: Inference) -> Self {
        Self { inference }
    }
}

#[async_trait]
impl InternalTool for RawInferenceTool {
    tools::tool_metadata!(
        "raw_inference",
        InferenceRequest,
        "Call a model directly with Ironpaw-native messages: one completion, \
             no history, no tool execution. Use it to delegate a task to a \
             specific model (e.g. a vision or audio model)."
    );

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &tools::ExecutionContext,
        responder: ToolResponder,
    ) {
        let result = async {
            let request =
                serde_json::from_value(args).map_err(|error| ToolError::new(error.to_string()))?;
            let output = self
                .inference
                .execute(request)
                .await
                .map_err(|error| ToolError::new(error.to_string()))?;
            serde_json::to_value(output).map_err(|error| ToolError::new(error.to_string()))
        }
        .await;
        responder.respond(result);
    }
}
