use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::tools::{self, InternalTool, ToolContext, ToolResponder};

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
    fn name(&self) -> &str {
        "todo"
    }

    fn tool_definition(&self) -> crate::message::ToolDefinition {
        tools::tool_definition::<TodoInput>(
            "todo",
            "Track the task list for this session. \
            Call this proactively for any multi-step task (3+ steps). \
            Write ALL planned tasks upfront, then update status in real-time: \
            mark in_progress before starting each task, completed immediately after. \
            Keep exactly one task in_progress at a time. \
            Do not stop until every task is completed.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
        responder: ToolResponder,
    ) -> Result<(), String> {
        let input: TodoInput = serde_json::from_value(args).map_err(|e| e.to_string())?;
        let key = ctx
            .conversation
            .as_ref()
            .map(|conversation| conversation.storage_key())
            .unwrap_or_else(|| "default".to_string());
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

        responder.respond(serde_json::json!({
            "ok": true,
            "old_todos": old_todos,
            "new_todos": input.todos,
            "result": lines.join("\n")
        }))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(description: &str, status: TodoStatus) -> serde_json::Value {
        serde_json::json!({ "description": description, "status": status })
    }

    #[tokio::test]
    async fn returns_previous_todos_for_conversation() {
        let tool = Arc::new(TodoTool::new());
        let ctx = ToolContext {
            conversation: Some(crate::bus::ConversationKey {
                channel: "telegram".to_string(),
                chat_id: "chat-1".to_string(),
                thread_id: None,
            }),
            ..ToolContext::default()
        };

        let first = tools::run_internal_tool(
            Arc::clone(&tool) as Arc<dyn InternalTool>,
            serde_json::json!({ "todos": [item("Plan", TodoStatus::InProgress)] }),
            &ctx,
        )
        .await;
        assert_eq!(first["old_todos"], serde_json::json!([]));

        let second = tools::run_internal_tool(
            Arc::clone(&tool) as Arc<dyn InternalTool>,
            serde_json::json!({ "todos": [item("Plan", TodoStatus::Completed)] }),
            &ctx,
        )
        .await;
        assert_eq!(
            second["old_todos"],
            serde_json::json!([{ "description": "Plan", "status": "in_progress" }])
        );
    }

    #[tokio::test]
    async fn completed_lists_clear_stored_todos() {
        let tool = Arc::new(TodoTool::new());
        let ctx = ToolContext {
            conversation: Some(crate::bus::ConversationKey {
                channel: "telegram".to_string(),
                chat_id: "chat-1".to_string(),
                thread_id: None,
            }),
            ..ToolContext::default()
        };

        tools::run_internal_tool(
            Arc::clone(&tool) as Arc<dyn InternalTool>,
            serde_json::json!({ "todos": [item("Plan", TodoStatus::InProgress)] }),
            &ctx,
        )
        .await;
        tools::run_internal_tool(
            Arc::clone(&tool) as Arc<dyn InternalTool>,
            serde_json::json!({ "todos": [item("Plan", TodoStatus::Completed)] }),
            &ctx,
        )
        .await;
        let next = tools::run_internal_tool(
            Arc::clone(&tool) as Arc<dyn InternalTool>,
            serde_json::json!({ "todos": [item("Next", TodoStatus::Pending)] }),
            &ctx,
        )
        .await;

        assert_eq!(next["old_todos"], serde_json::json!([]));
    }
}
