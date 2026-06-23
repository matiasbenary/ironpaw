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
    /// '2026-06-11T09:00:00+02:00'. Exactly one of `at` or `cron` must be set.
    #[serde(default)]
    pub at: Option<String>,
    /// Recurring schedule as a standard 5-field cron expression.
    #[serde(default)]
    pub cron: Option<String>,
    /// Name of the tool to run when the automation fires.
    #[serde(default)]
    pub tool: Option<String>,
    /// Arguments the tool is called with, matching its schema.
    #[serde(default)]
    pub args: Option<serde_json::Value>,
    /// Prompt for an agent task automation.
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

pub enum AutomationToolKind {
    Create,
    Delete,
    Inspect,
    List,
}

pub struct AutomationTool {
    bus: Bus,
    kind: AutomationToolKind,
}

impl AutomationTool {
    pub fn new(bus: Bus, kind: AutomationToolKind) -> Self {
        Self { bus, kind }
    }

    async fn request(&self, action: AutomationAction) -> Result<serde_json::Value, String> {
        let id = uuid::Uuid::new_v4().to_string();
        let mut events = self.bus.subscribe();
        self.bus.publish(Event::Automation {
            id: id.clone(),
            action,
        });
        loop {
            match events.recv().await {
                Ok(Event::AutomationResult {
                    id: response_id,
                    result,
                }) if response_id == id => return result,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    return Err("automation bus closed before response".to_string());
                }
            }
        }
    }

    async fn create_automation(
        &self,
        params: CreateAutomationParams,
    ) -> Result<serde_json::Value, String> {
        let trigger = match (
            params.at.filter(|datetime| !datetime.trim().is_empty()),
            params
                .cron
                .filter(|expression| !expression.trim().is_empty()),
        ) {
            (Some(datetime), None) => AutomationTriggerSpec::At { datetime },
            (None, Some(expression)) => AutomationTriggerSpec::Cron { expression },
            _ => return Err("set exactly one of at or cron".to_string()),
        };
        let effect = match (
            params.tool.filter(|tool| !tool.trim().is_empty()),
            params
                .agent_prompt
                .filter(|prompt| !prompt.trim().is_empty()),
        ) {
            (Some(tool), None) => AutomationEffect::Tool {
                tool,
                args: parse_args(params.args)?,
            },
            (None, Some(prompt)) if !params.agent_tools.is_empty() => AutomationEffect::AgentTask {
                prompt,
                tools: params.agent_tools,
            },
            (None, Some(_)) => {
                return Err("agent_tools must not be empty for an agent task".to_string());
            }
            _ => return Err("set exactly one of tool or agent_prompt".to_string()),
        };
        self.request(AutomationAction::Create {
            automation: AutomationSpec {
                name: params.name,
                trigger,
                effect,
            },
        })
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
impl InternalTool for AutomationTool {
    fn name(&self) -> &str {
        match self.kind {
            AutomationToolKind::Create => "create_automation",
            AutomationToolKind::Delete => "delete_automation",
            AutomationToolKind::Inspect => "inspect_automation",
            AutomationToolKind::List => "list_automations",
        }
    }

    fn tool_definition(&self) -> ToolDefinition {
        match self.kind {
            AutomationToolKind::Create => tools::tool_definition::<CreateAutomationParams>(
                self.name(),
                "Create an automation: a tool call that runs later — once at an absolute time, or repeatedly on a cron schedule.",
            ),
            AutomationToolKind::Delete => tools::tool_definition::<DeleteAutomationParams>(
                self.name(),
                "Delete an existing automation by exact name.",
            ),
            AutomationToolKind::Inspect => tools::tool_definition::<InspectAutomationParams>(
                self.name(),
                "Inspect an automation by exact name.",
            ),
            AutomationToolKind::List => tools::tool_definition::<ListAutomationsParams>(
                self.name(),
                "List the currently registered automations.",
            ),
        }
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolOutput, String> {
        match self.kind {
            AutomationToolKind::Create => {
                tools::execute_typed(args, |params| self.create_automation(params)).await
            }
            AutomationToolKind::Delete => {
                tools::execute_typed(args, |params: DeleteAutomationParams| async {
                    self.request(AutomationAction::Delete { name: params.name })
                        .await
                })
                .await
            }
            AutomationToolKind::Inspect => {
                tools::execute_typed(args, |params: InspectAutomationParams| async {
                    self.request(AutomationAction::Inspect { name: params.name })
                        .await
                })
                .await
            }
            AutomationToolKind::List => {
                tools::execute_typed(args, |_params: ListAutomationsParams| async {
                    self.request(AutomationAction::List).await
                })
                .await
            }
        }
    }
}
