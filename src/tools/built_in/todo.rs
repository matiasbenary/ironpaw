use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::tools::{self, ExecutionContext, InternalTool, ToolResult};

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    /// Task not yet started.
    Pending,
    /// Currently working on this (at most one at a time).
    InProgress,
    /// Finished successfully.
    Completed,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, JsonSchema)]
pub struct TodoItem {
    #[schemars(description = "Short imperative description of the task, e.g. 'Generate image'.")]
    pub description: String,
    pub status: TodoStatus,
}

#[derive(Deserialize, JsonSchema)]
struct TodoInput {
    /// The complete, updated todo list for this session.
    todos: Vec<TodoItem>,
}

#[derive(Clone)]
pub struct TodoTool {
    state: Arc<Mutex<HashMap<String, Vec<TodoItem>>>>,
}

impl TodoTool {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl Default for TodoTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl InternalTool for TodoTool {
    tools::tool_metadata!(
        "todo",
        TodoInput,
        "Track the task list for this session. \
            Call this proactively for any multi-step task (3+ steps). \
            Write ALL planned tasks upfront, then update status in real-time: \
            mark in_progress before starting each task, completed immediately after. \
            Keep exactly one task in_progress at a time. \
            Do not stop until every task is completed."
    );

    async fn execute(&self, args: serde_json::Value, ctx: &ExecutionContext) -> ToolResult {
        (|| {
            let input: TodoInput = serde_json::from_value(args).map_err(|e| e.to_string())?;
            let key = ctx.files_workspace().to_string_lossy().into_owned();
            let all_done = input
                .todos
                .iter()
                .all(|todo| matches!(todo.status, TodoStatus::Completed));
            let old_todos = {
                let mut state = self.state.lock().expect("todo state lock");
                let old_todos = state.remove(&key).unwrap_or_default();
                if !all_done {
                    state.insert(key, input.todos.clone());
                }
                old_todos
            };
            let mut lines = vec!["Todo list:".to_string()];
            for todo in &input.todos {
                let icon = match todo.status {
                    TodoStatus::Completed => "✓",
                    TodoStatus::InProgress => "→",
                    TodoStatus::Pending => "○",
                };
                lines.push(format!("  [{icon}] {}", todo.description));
            }

            let has_remaining = input
                .todos
                .iter()
                .any(|t| !matches!(t.status, TodoStatus::Completed));

            lines.push(String::new());
            if has_remaining {
                lines.push(
                "Continue with the next pending task. Do not stop until all tasks are completed."
                    .to_string(),
            );
            } else {
                lines.push("All tasks completed.".to_string());
            }

            Ok::<_, String>(serde_json::json!({
                "old_todos": old_todos,
                "new_todos": input.todos,
                "result": lines.join("\n")
            }))
        })()
        .map_err(crate::tools::ToolError::new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(description: &str, status: TodoStatus) -> serde_json::Value {
        serde_json::json!({ "description": description, "status": status })
    }

    #[tokio::test]
    async fn completed_lists_clear_stored_todos() {
        let tool = Arc::new(TodoTool::new());
        let ctx = ExecutionContext::new(std::path::PathBuf::from("conversation"));

        let _ = tools::execute_resolved_tool(
            Arc::clone(&tool) as Arc<dyn InternalTool>,
            serde_json::json!({ "todos": [item("Plan", TodoStatus::InProgress)] }),
            &ctx,
        )
        .await;
        let _ = tools::execute_resolved_tool(
            Arc::clone(&tool) as Arc<dyn InternalTool>,
            serde_json::json!({ "todos": [item("Plan", TodoStatus::Completed)] }),
            &ctx,
        )
        .await;
        let next = tools::execute_resolved_tool(
            Arc::clone(&tool) as Arc<dyn InternalTool>,
            serde_json::json!({ "todos": [item("Next", TodoStatus::Pending)] }),
            &ctx,
        )
        .await;

        assert_eq!(next.unwrap()["old_todos"], serde_json::json!([]));
    }
}
