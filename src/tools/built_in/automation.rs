use async_trait::async_trait;

use crate::{
    automations::AutomationEffect,
    bus::{AutomationAction, AutomationSpec, AutomationTriggerSpec, Bus, Event},
    message::ToolDefinition,
    tools::{self, ExecutionContext, InternalTool, ToolResponder},
};

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct CreateAutomationParams {
    /// Short kebab-case name for the automation, e.g. 'daily-news'.
    pub name: String,
    /// Run once at an absolute local time, RFC 3339 format. Set exactly one of `at` or `cron`.
    #[serde(default)]
    pub at: Option<String>,
    /// Recurring 5-field cron expression, or 6/7 fields with seconds.
    #[serde(default)]
    pub cron: Option<String>,
    /// Tool name to run when the automation fires. Set exactly one of this or `agent_prompt`.
    #[serde(default)]
    pub tool: Option<String>,
    /// Arguments for `tool`, matching that tool's schema.
    #[serde(default)]
    pub args: Option<serde_json::Value>,
    /// Prompt for an agent task that runs on each trigger.
    #[serde(default)]
    pub agent_prompt: Option<String>,
    /// Additional tools available to the agent task.
    #[serde(default)]
    pub agent_tools: Vec<String>,
}

pub type EditAutomationParams = CreateAutomationParams;

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

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct TriggerAutomationParams {
    /// Exact automation name to trigger.
    pub automation_id: String,
}

pub enum AutomationToolKind {
    Create,
    Edit,
    Delete,
    Inspect,
    List,
    Trigger,
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

    fn spec(params: CreateAutomationParams) -> Result<AutomationSpec, String> {
        let trigger = match (
            params.at.filter(|value| !value.trim().is_empty()),
            params.cron.filter(|value| !value.trim().is_empty()),
        ) {
            (Some(datetime), None) => AutomationTriggerSpec::At { datetime },
            (None, Some(expression)) => AutomationTriggerSpec::Cron { expression },
            _ => return Err("set exactly one of at or cron".to_string()),
        };
        let effect = match (
            params.tool.filter(|value| !value.trim().is_empty()),
            params.agent_prompt.filter(|value| !value.trim().is_empty()),
        ) {
            (Some(tool), None) => AutomationEffect::Tool {
                tool,
                args: parse_args(params.args)?,
            },
            (None, Some(prompt)) => AutomationEffect::AgentTask {
                prompt,
                tools: params.agent_tools,
            },
            _ => return Err("set exactly one of tool or agent_prompt".to_string()),
        };
        Ok(AutomationSpec {
            name: params.name,
            trigger,
            effect,
        })
    }
}

fn parse_args(args: Option<serde_json::Value>) -> Result<serde_json::Value, String> {
    match args {
        None => Ok(serde_json::json!({})),
        Some(serde_json::Value::String(text)) => serde_json::from_str(&text)
            .map_err(|err| format!("args must be JSON, got a string: {err}")),
        Some(value) => Ok(value),
    }
}

#[async_trait]
impl InternalTool for AutomationTool {
    fn name(&self) -> &str {
        match self.kind {
            AutomationToolKind::Create => "create_automation",
            AutomationToolKind::Edit => "edit_automation",
            AutomationToolKind::Delete => "delete_automation",
            AutomationToolKind::Inspect => "inspect_automation",
            AutomationToolKind::List => "list_automations",
            AutomationToolKind::Trigger => "trigger_automation",
        }
    }

    fn tool_definition(&self) -> ToolDefinition {
        match self.kind {
            AutomationToolKind::Create => tools::tool_definition::<CreateAutomationParams>(
                self.name(),
                "Create a tool-call or agent-task automation that runs once at an absolute time or repeatedly on a cron schedule.",
            ),
            AutomationToolKind::Edit => tools::tool_definition::<EditAutomationParams>(
                self.name(),
                "Replace an existing automation with the supplied complete definition. Use inspect_automation first when changing an existing automation.",
            ),
            AutomationToolKind::Delete => tools::tool_definition::<DeleteAutomationParams>(
                self.name(),
                "Delete an automation by exact name.",
            ),
            AutomationToolKind::Inspect => tools::tool_definition::<InspectAutomationParams>(
                self.name(),
                "Inspect an automation by exact name.",
            ),
            AutomationToolKind::List => tools::tool_definition::<ListAutomationsParams>(
                self.name(),
                "List registered automations.",
            ),
            AutomationToolKind::Trigger => tools::tool_definition::<TriggerAutomationParams>(
                self.name(),
                "Trigger an automation by exact name.",
            ),
        }
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ExecutionContext,
        responder: ToolResponder,
    ) {
        let result = match self.kind {
            AutomationToolKind::Create => {
                tools::execute_typed(args, |params| async {
                    self.request(AutomationAction::Create {
                        automation: Self::spec(params)?,
                    })
                    .await
                })
                .await
            }
            AutomationToolKind::Edit => {
                tools::execute_typed(args, |params| async {
                    self.request(AutomationAction::Edit {
                        automation: Self::spec(params)?,
                    })
                    .await
                })
                .await
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
            AutomationToolKind::Trigger => {
                tools::execute_typed(args, |params: TriggerAutomationParams| async {
                    self.request(AutomationAction::Trigger {
                        name: params.automation_id,
                    })
                    .await
                })
                .await
            }
        };
        responder.respond(result);
    }
}
