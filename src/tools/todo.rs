use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    openai::Tool,
    tools::{self, InternalTool, ToolOutput},
};

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    /// Task not yet started.
    Pending,
    /// Currently working on this (at most one at a time).
    InProgress,
    /// Finished successfully.
    Completed,
}

#[derive(Serialize, Deserialize, Clone, Debug, JsonSchema)]
pub struct TodoItem {
    #[schemars(description = "Short imperative description of the task, e.g. 'Generate image'.")]
    pub description: String,
    pub status: TodoStatus,
}

#[derive(Deserialize, JsonSchema)]
struct TodoInput {
    /// The complete, updated todo list for this session.
    todos: Vec<TodoItem>,
    /// Injected by the engine — not part of the model schema.
    #[serde(default)]
    #[schemars(skip)]
    pub conversation_id: Option<String>,
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

#[async_trait]
impl InternalTool for TodoTool {
    fn name(&self) -> &'static str {
        "todo"
    }

    fn responses_schema(&self) -> Tool {
        tools::responses_tool_schema::<TodoInput>(
            "todo",
            "Track the task list for this session. \
            Call this proactively for any multi-step task (3+ steps). \
            Write ALL planned tasks upfront, then update status in real-time: \
            mark in_progress before starting each task, completed immediately after. \
            Keep exactly one task in_progress at a time. \
            Do not stop until every task is completed.",
        )
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput, String> {
        let input: TodoInput = serde_json::from_value(args).map_err(|e| e.to_string())?;
        let key = input
            .conversation_id
            .unwrap_or_else(|| "default".to_string());

        let mut state = self.state.lock().unwrap();
        state.insert(key, input.todos.clone());

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

        if has_remaining {
            lines.push(String::new());
            lines.push(
                "Continue with the next pending task. Do not stop until all tasks are completed."
                    .to_string(),
            );
        } else {
            lines.push(String::new());
            lines.push("All tasks completed.".to_string());
        }

        Ok(ToolOutput::json(serde_json::json!({
            "result": lines.join("\n")
        })))
    }
}
