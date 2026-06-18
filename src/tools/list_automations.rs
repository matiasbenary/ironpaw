use std::sync::{Arc, OnceLock};

use async_trait::async_trait;

use crate::{
    automations::Automations,
    message::ToolDefinition,
    tools::{self, InternalTool, ToolContext, ToolOutput},
};

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ListAutomationsParams {}

#[derive(Default)]
pub struct ListAutomationsTool {
    automations: OnceLock<Arc<Automations>>,
}

impl ListAutomationsTool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bind(&self, automations: Arc<Automations>) {
        let _ = self.automations.set(automations);
    }

    pub async fn execute(
        &self,
        _params: ListAutomationsParams,
    ) -> Result<serde_json::Value, String> {
        let automations = self
            .automations
            .get()
            .ok_or_else(|| "automations are not available".to_string())?;
        serde_json::to_value(automations.list()).map_err(|err| err.to_string())
    }
}

#[async_trait]
impl InternalTool for ListAutomationsTool {
    fn name(&self) -> &str {
        "list_automations"
    }

    fn tool_definition(&self) -> ToolDefinition {
        tools::tool_definition::<ListAutomationsParams>(
            self.name(),
            "List the currently registered automations.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput, String> {
        tools::execute_typed(args, |params: ListAutomationsParams| async {
            ListAutomationsTool::execute(self, params).await
        })
        .await
        .map(ToolOutput::json)
    }
}
