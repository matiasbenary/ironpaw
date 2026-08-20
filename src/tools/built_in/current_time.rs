use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    clock::Clock,
    tools::{self, ExecutionContext, InternalTool, ToolResult},
};

pub struct CurrentTimeTool {
    clock: Arc<dyn Clock>,
}

impl CurrentTimeTool {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self { clock }
    }
}

#[derive(schemars::JsonSchema, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CurrentTimeInput {}

#[async_trait]
impl InternalTool for CurrentTimeTool {
    tools::tool_metadata!(
        "current_time",
        CurrentTimeInput,
        "Return the current date and time in Ironpaw's local timezone."
    );

    async fn execute(&self, _args: serde_json::Value, _ctx: &ExecutionContext) -> ToolResult {
        let now = self.clock.now().with_timezone(&chrono::Local);
        Ok(serde_json::json!({
            "datetime": now.to_rfc3339(),
            "utc_offset": now.format("%:z").to_string(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::execute_resolved_tool;

    struct FixedClock(chrono::DateTime<chrono::Utc>);

    impl Clock for FixedClock {
        fn now(&self) -> chrono::DateTime<chrono::Utc> {
            self.0
        }
    }

    #[tokio::test]
    async fn returns_the_shared_clock_time() {
        let expected = "2026-08-20T20:37:23Z"
            .parse::<chrono::DateTime<chrono::Utc>>()
            .unwrap();
        let result = execute_resolved_tool(
            Arc::new(CurrentTimeTool::new(Arc::new(FixedClock(expected)))),
            serde_json::json!({}),
            &ExecutionContext::new(std::path::PathBuf::from("test")),
        )
        .await
        .unwrap();
        let actual = result["datetime"]
            .as_str()
            .unwrap()
            .parse::<chrono::DateTime<chrono::FixedOffset>>()
            .unwrap();

        assert_eq!(actual.to_utc(), expected);
        assert_eq!(result["utc_offset"], actual.format("%:z").to_string());
    }
}
