use async_trait::async_trait;

use crate::{
    bus::{AutomationAction, Bus, Event},
    message::ToolDefinition,
    tools::{self, ExecutionContext, InternalTool, ToolResponder},
};

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct RegisterAutomationParams {
    /// Name of the JSON definition in automations/<name>.json to validate and register.
    pub name: String,
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

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct TriggerAutomationParams {
    /// Exact automation name to trigger.
    pub automation_id: String,
}

pub enum AutomationToolKind {
    Register,
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
            AutomationToolKind::Register => "register_automation",
            AutomationToolKind::Delete => "delete_automation",
            AutomationToolKind::Inspect => "inspect_automation",
            AutomationToolKind::List => "list_automations",
            AutomationToolKind::Trigger => "trigger_automation",
        }
    }

    fn tool_definition(&self) -> ToolDefinition {
        match self.kind {
            AutomationToolKind::Register => tools::tool_definition::<RegisterAutomationParams>(
                self.name(),
                "Validate automations/<name>.json and register it as a live automation. Editing the file alone does not change a running automation.",
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
            AutomationToolKind::Trigger => tools::tool_definition::<TriggerAutomationParams>(
                self.name(),
                "Trigger an existing automation by exact id. Returns once the execution has been dispatched.",
            ),
        }
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ExecutionContext,
        responder: ToolResponder,
    ) {
        let output = match self.kind {
            AutomationToolKind::Register => {
                tools::execute_typed(args, |params: RegisterAutomationParams| async {
                    self.request(AutomationAction::Register { name: params.name })
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
        responder.respond(output);
    }
}
