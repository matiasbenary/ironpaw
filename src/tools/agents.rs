use async_trait::async_trait;

use crate::{
    bus::{AgentRequest, Bus, ChannelAddress, Event},
    message::ToolDefinition,
    tools::{self, InternalTool, ToolContext, ToolOutput},
};

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CallAgentParams {
    /// The task or message for the agent to handle.
    pub task: String,
}

/// Runs a sub-agent: same tools, its own conversation, final answer returned
/// as the tool result.
pub struct CallAgentTool {
    bus: Bus,
}

impl CallAgentTool {
    pub fn new(bus: Bus) -> Self {
        Self { bus }
    }

    pub async fn execute(
        &self,
        params: CallAgentParams,
        ctx: &crate::tools::ToolContext,
    ) -> Result<serde_json::Value, String> {
        // The sub-agent inherits the caller's destination so its progress
        // and channel defaults behave like the run that spawned it.
        let destination = ctx.destination.clone();
        let result = request_agent(
            &self.bus,
            destination,
            AgentRequest::RunTask { task: params.task },
        )
        .await?;
        Ok(serde_json::json!({ "ok": true, "result": result }))
    }
}

pub(crate) async fn request_agent(
    bus: &Bus,
    destination: Option<(String, ChannelAddress)>,
    request: AgentRequest,
) -> Result<Option<String>, String> {
    let request_id = uuid::Uuid::new_v4().to_string();
    let mut events = bus.subscribe();
    bus.publish(Event::AgentRequest {
        request_id: request_id.clone(),
        destination,
        request,
    });
    loop {
        match events.recv().await {
            Ok(Event::AgentResponse {
                request_id: response_id,
                response,
            }) if response_id == request_id => return response,
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                return Err("agent bus closed before response".to_string());
            }
        }
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
             task described in `task` and its final answer is returned as \
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
