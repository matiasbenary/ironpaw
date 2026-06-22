use async_trait::async_trait;

use crate::{
    bus::{AutomationAction, Bus, Event},
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
        request_automation(
            &self.bus,
            AutomationAction::Create {
                name: params.name,
                at: params.at,
                cron: params.cron,
                tool: params.tool,
                args: params.args,
            },
        )
        .await
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
        .map(ToolOutput::json)
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
        .map(ToolOutput::json)
    }
}
