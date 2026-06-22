use async_trait::async_trait;

use crate::{
    bus::{AgentRequest, Bus},
    message::ToolDefinition,
    tools::{self, InternalTool, ToolContext, ToolOutput, call_agent::request_agent},
};

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContinueConversationParams {
    /// Conversation whose active history should be handled.
    pub conversation_id: String,
}

pub struct ContinueConversationTool {
    bus: Bus,
}

impl ContinueConversationTool {
    pub fn new(bus: Bus) -> Self {
        Self { bus }
    }

    pub async fn execute(
        &self,
        params: ContinueConversationParams,
        ctx: &ToolContext,
    ) -> Result<serde_json::Value, String> {
        let result = request_agent(
            &self.bus,
            ctx.destination.clone(),
            AgentRequest::ContinueConversation {
                conversation_id: params.conversation_id,
            },
        )
        .await?;
        Ok(serde_json::json!({ "ok": true, "result": result }))
    }
}

#[async_trait]
impl InternalTool for ContinueConversationTool {
    fn name(&self) -> &str {
        "continue_conversation"
    }

    fn tool_definition(&self) -> ToolDefinition {
        tools::tool_definition::<ContinueConversationParams>(
            self.name(),
            "Continue an existing conversation using its active history.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, String> {
        tools::execute_typed(args, |params: ContinueConversationParams| async {
            ContinueConversationTool::execute(self, params, ctx).await
        })
        .await
        .map(ToolOutput::json)
    }
}
