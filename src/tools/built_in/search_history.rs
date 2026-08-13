use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    history::History,
    tools::{self, ExecutionContext, InternalTool, ToolResponder},
};

#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchHistoryParams {
    /// Text to search for in Ironpaw's stored history.
    pub query: String,
}

pub struct SearchHistoryTool {
    history: Arc<dyn History>,
}

impl SearchHistoryTool {
    pub fn new(history: Arc<dyn History>) -> Self {
        Self { history }
    }

    pub fn execute(&self, params: SearchHistoryParams) -> Result<serde_json::Value, String> {
        let query = params.query.trim();
        if query.is_empty() {
            return Err("query is required".to_string());
        }
        Ok(serde_json::json!({
            "ok": true,
            "results": self.history.search(query)?,
        }))
    }
}

#[async_trait]
impl InternalTool for SearchHistoryTool {
    tools::tool_metadata!(
        "search_history",
        SearchHistoryParams,
        "Search Ironpaw's stored history across all conversations."
    );

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ExecutionContext,
        responder: ToolResponder,
    ) {
        let result = serde_json::from_value(args)
            .map_err(|err| err.to_string())
            .and_then(|params| SearchHistoryTool::execute(self, params))
            .map_err(crate::tools::ToolError::new);
        responder.respond(result);
    }
}
