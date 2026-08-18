use async_trait::async_trait;

use crate::{
    automations::Automation,
    bus::{AutomationAction, Bus, Event},
    message::ToolDefinition,
    tools::{self, ExecutionContext, InternalTool, ToolResult},
};

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct NamedAutomationParams {
    /// Exact automation name.
    pub name: String,
}

pub type DeleteAutomationParams = NamedAutomationParams;
pub type InspectAutomationParams = NamedAutomationParams;

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
            AutomationToolKind::Create => tools::tool_definition::<Automation>(
                self.name(),
                "Create a scheduled or inbound-message automation.",
            ),
            AutomationToolKind::Edit => tools::tool_definition::<Automation>(
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

    async fn execute(&self, args: serde_json::Value, _ctx: &ExecutionContext) -> ToolResult {
        let result = match self.kind {
            AutomationToolKind::Create => {
                tools::execute_typed(args, |automation| async {
                    self.request(AutomationAction::Create { automation }).await
                })
                .await
            }
            AutomationToolKind::Edit => {
                tools::execute_typed(args, |automation| async {
                    self.request(AutomationAction::Edit { automation }).await
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
        result
    }
}
