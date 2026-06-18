use std::sync::{Arc, OnceLock};

use async_trait::async_trait;

use crate::{
    automations::Automations,
    message::ToolDefinition,
    tools::{self, InternalTool, ToolContext, ToolOutput},
};

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct InspectAutomationParams {
    /// Exact automation name to inspect.
    pub name: String,
}

#[derive(Default)]
pub struct InspectAutomationTool {
    automations: OnceLock<Arc<Automations>>,
}

impl InspectAutomationTool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bind(&self, automations: Arc<Automations>) {
        let _ = self.automations.set(automations);
    }

    pub async fn execute(
        &self,
        params: InspectAutomationParams,
    ) -> Result<serde_json::Value, String> {
        let automations = self
            .automations
            .get()
            .ok_or_else(|| "automations are not available".to_string())?;
        let name = params.name.trim();
        if name.is_empty() {
            return Err("name must not be empty".to_string());
        }
        serde_json::to_value(automations.inspect(name)?).map_err(|err| err.to_string())
    }
}

#[async_trait]
impl InternalTool for InspectAutomationTool {
    fn name(&self) -> &str {
        "inspect_automation"
    }

    fn tool_definition(&self) -> ToolDefinition {
        tools::tool_definition::<InspectAutomationParams>(
            self.name(),
            "Inspect an automation by exact name. Returns its trigger, target \
             tool, args, and enabled state. Durable run logs are not implemented yet.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput, String> {
        tools::execute_typed(args, |params: InspectAutomationParams| async {
            InspectAutomationTool::execute(self, params).await
        })
        .await
        .map(ToolOutput::json)
    }
}
