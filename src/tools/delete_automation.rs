use std::sync::{Arc, OnceLock};

use async_trait::async_trait;

use crate::{
    automations::Automations,
    message::ToolDefinition,
    tools::{self, InternalTool, ToolContext, ToolOutput},
};

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct DeleteAutomationParams {
    /// Exact automation name to delete.
    pub name: String,
}

#[derive(Default)]
pub struct DeleteAutomationTool {
    automations: OnceLock<Arc<Automations>>,
}

impl DeleteAutomationTool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bind(&self, automations: Arc<Automations>) {
        let _ = self.automations.set(automations);
    }

    pub async fn execute(
        &self,
        params: DeleteAutomationParams,
    ) -> Result<serde_json::Value, String> {
        let automations = self
            .automations
            .get()
            .ok_or_else(|| "automations are not available".to_string())?;
        let name = params.name.trim();
        if name.is_empty() {
            return Err("name must not be empty".to_string());
        }
        automations.delete(name)?;
        Ok(serde_json::json!({ "ok": true, "name": name }))
    }
}

#[async_trait]
impl InternalTool for DeleteAutomationTool {
    fn name(&self) -> &str {
        "delete_automation"
    }

    fn tool_definition(&self) -> ToolDefinition {
        tools::tool_definition::<DeleteAutomationParams>(
            self.name(),
            "Delete an existing automation by exact name. Use this when the \
             user asks to cancel, remove, or stop a scheduled automation.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput, String> {
        tools::execute_typed(args, |params: DeleteAutomationParams| async {
            DeleteAutomationTool::execute(self, params).await
        })
        .await
        .map(ToolOutput::json)
    }
}
