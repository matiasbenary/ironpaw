use std::sync::{Arc, OnceLock};

use async_trait::async_trait;

use crate::{
    automations::{Automation, Automations, Trigger},
    message::ToolDefinition,
    tools::{self, InternalTool, ToolContext, ToolOutput},
};

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct CreateAutomationParams {
    /// Short kebab-case name for the automation, e.g. 'daily-news'.
    pub name: String,
    /// Run once at an absolute local time, RFC 3339 format, e.g.
    /// '2026-06-11T09:00:00+02:00'. Use for one-shot requests — "tomorrow
    /// at 9", "next Monday", "in 30 seconds" — computing the timestamp
    /// from the current time given in your instructions. Exactly one of
    /// `at` or `cron` must be set.
    #[serde(default)]
    pub at: Option<String>,
    /// Recurring schedule as a standard 5-field cron expression
    /// (min hour day-of-month month day-of-week), evaluated in local time,
    /// e.g. '0 9 * * *' for every day at 09:00.
    #[serde(default)]
    pub cron: Option<String>,
    /// Name of the tool to run when the automation fires. Any existing tool
    /// works, including tools you created with create_tool. For multi-step
    /// logic, create a tool first and reference it here.
    pub tool: String,
    /// Arguments the tool is called with, matching its schema. Automations
    /// run outside any conversation, so include explicit `channel` and
    /// `conversation_id` when the tool is send_message. Example:
    /// {"channel": "tui", "conversation_id": "abc", "content": "hi"}
    #[serde(default)]
    pub args: Option<serde_json::Value>,
}

/// Lets the model install automations: a trigger (one-shot delay or cron)
/// plus a script. Bound to the automations runtime after construction.
#[derive(Default)]
pub struct CreateAutomationTool {
    automations: OnceLock<Arc<Automations>>,
}

impl CreateAutomationTool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bind(&self, automations: Arc<Automations>) {
        let _ = self.automations.set(automations);
    }

    pub async fn execute(
        &self,
        params: CreateAutomationParams,
    ) -> Result<serde_json::Value, String> {
        let automations = self
            .automations
            .get()
            .ok_or_else(|| "automations are not available".to_string())?;
        let cron = params
            .cron
            .as_deref()
            .filter(|expression| !expression.trim().is_empty());
        let at = params
            .at
            .as_deref()
            .filter(|datetime| !datetime.trim().is_empty());
        let trigger = match (at, cron) {
            (Some(datetime), None) => Trigger::at(datetime)?,
            (None, Some(expression)) => Trigger::cron(expression)?,
            _ => return Err("set exactly one of at or cron".to_string()),
        };
        if params.tool.trim().is_empty() {
            return Err("tool must not be empty".to_string());
        }
        // Models sometimes send args as a JSON-encoded string; unwrap it
        // here so the automation can never fail on it at fire time.
        let args = match params.args {
            None => serde_json::json!({}),
            Some(serde_json::Value::String(text)) => serde_json::from_str(&text)
                .map_err(|err| format!("args must be a JSON object, got a string: {err}"))?,
            Some(value) => value,
        };
        automations.validate_tool_call(&params.tool, &args)?;
        automations.add_persistent(Automation {
            name: params.name.clone(),
            enabled: true,
            trigger,
            tool: params.tool,
            args,
        })?;
        Ok(serde_json::json!({ "ok": true, "name": params.name }))
    }
}

#[async_trait]
impl InternalTool for CreateAutomationTool {
    fn name(&self) -> &str {
        "create_automation"
    }

    fn tool_definition(&self) -> ToolDefinition {
        tools::tool_definition::<CreateAutomationParams>(
            self.name(),
            "Create an automation: a tool call that runs later — once at an \
             absolute time, or repeatedly on a cron schedule. Use this \
             whenever the user asks for something to happen at a later time \
             or on a schedule. For anything beyond a single tool call, \
             first create a tool with create_tool, then schedule it here.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput, String> {
        tools::execute_typed(args, |params: CreateAutomationParams| async {
            CreateAutomationTool::execute(self, params).await
        })
        .await
        .map(ToolOutput::json)
    }
}
