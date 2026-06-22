use async_trait::async_trait;

use crate::{
    automations::AutomationEffect,
    bus::{AutomationAction, AutomationSpec, AutomationTriggerSpec, Bus, Event},
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
    /// Name of the tool to run when the automation fires. Set this for a
    /// direct tool automation. For multi-step logic, create a tool first and
    /// reference it here.
    #[serde(default)]
    pub tool: Option<String>,
    /// Arguments the tool is called with, matching its schema.
    #[serde(default)]
    pub args: Option<serde_json::Value>,
    /// Prompt for an agent task automation. Set this instead of `tool` when
    /// the automation should let the agent solve a task.
    #[serde(default)]
    pub agent_prompt: Option<String>,
    /// Exact tools the agent task may use. Required with `agent_prompt`.
    #[serde(default)]
    pub agent_tools: Vec<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct DeleteAutomationParams {
    /// Exact automation name to delete.
    pub name: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct InspectAutomationParams {
    /// Exact automation name to inspect.
    pub name: String,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ListAutomationsParams {}

async fn request_automation(
    bus: &Bus,
    action: AutomationAction,
) -> Result<serde_json::Value, String> {
    let id = uuid::Uuid::new_v4().to_string();
    let mut events = bus.subscribe();
    bus.publish(Event::Automation {
        id: id.clone(),
        action,
    });
    loop {
        match events.recv().await {
            Ok(Event::AutomationResult {
                id: response_id,
                result,
            }) if response_id == id => return result,
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                return Err("automation bus closed before response".to_string());
            }
        }
    }
}

/// Lets the model install automations: a trigger (one-shot delay or cron)
/// plus a script.
pub struct CreateAutomationTool {
    bus: Bus,
}

impl CreateAutomationTool {
    pub fn new(bus: Bus) -> Self {
        Self { bus }
    }

    pub async fn execute(
        &self,
        params: CreateAutomationParams,
    ) -> Result<serde_json::Value, String> {
        let cron = params
            .cron
            .filter(|expression| !expression.trim().is_empty());
        let at = params.at.filter(|datetime| !datetime.trim().is_empty());
        let trigger = match (at, cron) {
            (Some(datetime), None) => AutomationTriggerSpec::At { datetime },
            (None, Some(expression)) => AutomationTriggerSpec::Cron { expression },
            _ => return Err("set exactly one of at or cron".to_string()),
        };
        let tool = params.tool.filter(|tool| !tool.trim().is_empty());
        let agent_prompt = params
            .agent_prompt
            .filter(|prompt| !prompt.trim().is_empty());
        let effect = match (tool, agent_prompt) {
            (Some(tool), None) => {
                let args = parse_args(params.args)?;
                AutomationEffect::Tool { tool, args }
            }
            (None, Some(prompt)) => {
                if params.agent_tools.is_empty() {
                    return Err("agent_tools must not be empty for an agent task".to_string());
                }
                AutomationEffect::AgentTask {
                    prompt,
                    tools: params.agent_tools,
                }
            }
            _ => return Err("set exactly one of tool or agent_prompt".to_string()),
        };
        request_automation(
            &self.bus,
            AutomationAction::Create {
                automation: AutomationSpec {
                    name: params.name,
                    trigger,
                    effect,
                },
            },
        )
        .await
    }
}

fn parse_args(args: Option<serde_json::Value>) -> Result<serde_json::Value, String> {
    match args {
        None => Ok(serde_json::json!({})),
        Some(serde_json::Value::String(text)) => serde_json::from_str(&text)
            .map_err(|err| format!("args must be a JSON object, got a string: {err}")),
        Some(value) => Ok(value),
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
    }
}

pub struct DeleteAutomationTool {
    bus: Bus,
}

impl DeleteAutomationTool {
    pub fn new(bus: Bus) -> Self {
        Self { bus }
    }

    pub async fn execute(
        &self,
        params: DeleteAutomationParams,
    ) -> Result<serde_json::Value, String> {
        request_automation(&self.bus, AutomationAction::Delete { name: params.name }).await
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
    }
}

pub struct InspectAutomationTool {
    bus: Bus,
}

impl InspectAutomationTool {
    pub fn new(bus: Bus) -> Self {
        Self { bus }
    }

    pub async fn execute(
        &self,
        params: InspectAutomationParams,
    ) -> Result<serde_json::Value, String> {
        request_automation(&self.bus, AutomationAction::Inspect { name: params.name }).await
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
             tool, and args. Durable run logs are not implemented yet.",
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
    }
}

pub struct ListAutomationsTool {
    bus: Bus,
}

impl ListAutomationsTool {
    pub fn new(bus: Bus) -> Self {
        Self { bus }
    }

    pub async fn execute(
        &self,
        _params: ListAutomationsParams,
    ) -> Result<serde_json::Value, String> {
        request_automation(&self.bus, AutomationAction::List).await
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
    }
}
