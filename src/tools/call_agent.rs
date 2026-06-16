use std::sync::{Arc, OnceLock};

use async_trait::async_trait;

use crate::{
    agent::Agent,
    message::{Message, ToolDefinition},
    tools::{self, InternalTool, ToolOutput},
};

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct CallAgentParams {
    /// The task or message for the agent to handle.
    pub message: String,
    /// Conversation to run in. Omit to run the agent on a fresh scratch
    /// conversation; its final answer is returned as the tool result.
    #[serde(default)]
    pub conversation_id: Option<String>,
}

/// Runs a sub-agent: same tools, its own conversation, final answer returned
/// as the tool result. Bound to the agent after construction because the
/// agent and the tool list need each other.
#[derive(Default)]
pub struct CallAgentTool {
    agent: OnceLock<Arc<Agent>>,
}

impl CallAgentTool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bind(&self, agent: Arc<Agent>) {
        let _ = self.agent.set(agent);
    }

    pub async fn execute(
        &self,
        params: CallAgentParams,
        ctx: &crate::tools::ToolContext,
    ) -> Result<serde_json::Value, String> {
        let agent = self
            .agent
            .get()
            .ok_or_else(|| "agent is not available".to_string())?;
        let conversation_id = params
            .conversation_id
            .unwrap_or_else(|| format!("agent-task-{}", uuid::Uuid::new_v4()));
        let messages = vec![Message::User {
            text: params.message,
            attachments: Vec::new(),
        }];
        // The sub-agent inherits the caller's destination so its progress
        // and channel defaults behave like the run that spawned it.
        let destination = ctx
            .destination
            .as_ref()
            .map(|(channel, address)| (channel.as_str(), address));
        let result = agent.run(destination, &conversation_id, messages).await?;
        Ok(serde_json::json!({ "ok": true, "result": result }))
    }
}

#[async_trait]
impl InternalTool for CallAgentTool {
    fn name(&self) -> &str {
        "call_agent"
    }

    fn tool_definition(&self) -> ToolDefinition {
        tools::tool_definition::<CallAgentParams>(
            self.name(),
            "Run a sub-agent with access to the same tools. It performs the \
             task described in `message` and its final answer is returned as \
             the tool result.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &crate::tools::ToolContext,
    ) -> Result<ToolOutput, String> {
        tools::execute_typed(args, |params: CallAgentParams| async {
            CallAgentTool::execute(self, params, ctx).await
        })
        .await
        .map(ToolOutput::json)
    }
}
