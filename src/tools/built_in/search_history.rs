use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    history::History,
    message::ToolDefinition,
    tools::{self, InternalTool, ToolContext},
};

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchHistoryParams {
    /// Text to search for in Warden's stored history.
    pub query: String,
}

pub struct SearchHistoryTool {
    history: Arc<dyn History>,
}

impl SearchHistoryTool {
    pub fn new(history: Arc<dyn History>) -> Self {
        Self { history }
    }

    pub async fn execute(&self, params: SearchHistoryParams) -> Result<serde_json::Value, String> {
        let query = params.query.trim();
        if query.is_empty() {
            return Err("query is required".to_string());
        }
        Ok(serde_json::json!({
            "ok": true,
            "results": self.history.search(query),
        }))
    }
}

#[async_trait]
impl InternalTool for SearchHistoryTool {
    fn name(&self) -> &str {
        "search_history"
    }

    fn tool_definition(&self) -> ToolDefinition {
        tools::tool_definition::<SearchHistoryParams>(
            self.name(),
            "Search Warden's stored history across all conversations.",
        )
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<(), String> {
        let output = tools::execute_typed(args, |params: SearchHistoryParams| async {
            SearchHistoryTool::execute(self, params).await
        })
        .await?;
        ctx.respond(output)?;
        Ok(())
    }
}
