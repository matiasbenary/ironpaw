use std::sync::{Arc, OnceLock};

use async_trait::async_trait;

use crate::{
    agent::Agent,
    message::ToolDefinition,
    tools::{self, InternalTool, ToolContext, ToolOutput},
};

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContinueConversationParams {
    /// Conversation whose active history should be handled.
    pub conversation_id: String,
}

#[derive(Default)]
pub struct ContinueConversationTool {
    agent: OnceLock<Arc<Agent>>,
}

impl ContinueConversationTool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bind(&self, agent: Arc<Agent>) {
        let _ = self.agent.set(agent);
    }

    pub async fn execute(
        &self,
        params: ContinueConversationParams,
        ctx: &ToolContext,
    ) -> Result<serde_json::Value, String> {
        let agent = self
            .agent
            .get()
            .ok_or_else(|| "agent is not available".to_string())?;
        let destination = ctx
            .destination
            .as_ref()
            .map(|(channel, address)| (channel.as_str(), address));
        let result = agent
            .handle_conversation(destination, &params.conversation_id)
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
