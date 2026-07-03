use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::{
    automations::{Automation, AutomationEffect, Automations, Trigger},
    bus::Sender,
    message::ToolDefinition,
    tools::{
        self, InternalTool, ToolContext, ToolResponder,
        setup::{SetupServices, delete_current_message, send},
    },
};

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct OnboardingParams {
    /// The latest user message.
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Greeting,
    NeedNear,
    Complete,
}

pub struct OnboardingTool {
    stage: Mutex<Stage>,
    setup: Arc<SetupServices>,
}

impl OnboardingTool {
    pub fn new(setup: Arc<SetupServices>) -> Self {
        Self {
            stage: Mutex::new(Stage::Greeting),
            setup,
        }
    }

    pub fn automation() -> Automation {
        Automation {
            name: "onboarding".to_string(),
            trigger: Trigger::MessageIn {
                sender: Some(Sender::User),
                channel: None,
            },
            effect: AutomationEffect::Tool {
                tool: "onboarding".to_string(),
                args: serde_json::json!({
                    "message": "{{event.text}}",
                }),
            },
        }
    }

    async fn execute(
        &self,
        params: OnboardingParams,
        ctx: &ToolContext,
    ) -> Result<serde_json::Value, String> {
        let stage = *self.stage.lock().expect("onboarding stage lock");
        match stage {
            Stage::Greeting => {
                *self.stage.lock().expect("onboarding stage lock") = Stage::NeedNear;
                send(
                    &self.setup.tools,
                    "Welcome to Warden. Let's set up the API key I need. Paste your NEAR API key.",
                    ctx,
                )
                .await?;
            }
            Stage::NeedNear => {
                let key = clean_key(&params.message)?;
                send(
                    &self.setup.tools,
                    "Thanks, let me redact that for you.",
                    ctx,
                )
                .await?;
                self.setup.secrets.set("NEAR_API_KEY", &key)?;
                delete_current_message(&self.setup.tools, ctx, "onboarding").await;
                send(
                    &self.setup.tools,
                    "Thanks, let me test the key to make sure it works.",
                    ctx,
                )
                .await?;
                if let Err(err) = self.test_near(ctx).await {
                    self.setup.secrets.remove("NEAR_API_KEY")?;
                    *self.stage.lock().expect("onboarding stage lock") = Stage::NeedNear;
                    send(
                        &self.setup.tools,
                        &format!(
                            "The NEAR API key did not pass a basic inference test: {err}. Paste your NEAR API key again."
                        ),
                        ctx,
                    )
                    .await?;
                    return Ok(serde_json::json!({ "ok": false, "error": err }));
                }
                self.finish(ctx).await?;
            }
            Stage::Complete => {
                send(&self.setup.tools, "Warden is already set up.", ctx).await?;
            }
        }
        Ok(serde_json::json!({ "ok": true }))
    }

    async fn test_near(&self, ctx: &ToolContext) -> Result<(), String> {
        let result = tools::run_tool(
            &self.setup.tools,
            "raw_inference",
            serde_json::json!({
                "messages": [
                    { "User": { "text": "Reply with exactly OK.", "attachments": [] } }
                ]
            }),
            ctx,
        )
        .await;
        tool_result_ok(result)
    }

    async fn finish(&self, ctx: &ToolContext) -> Result<(), String> {
        *self.stage.lock().expect("onboarding stage lock") = Stage::Complete;
        self.setup.automations.delete("onboarding")?;
        self.setup
            .automations
            .add(Automations::handle_conversation())?;
        send(
            &self.setup.tools,
            "Warden is set up. From now on, I will answer your messages normally.",
            ctx,
        )
        .await
    }
}

fn tool_result_ok(result: serde_json::Value) -> Result<(), String> {
    if result.get("ok").and_then(|ok| ok.as_bool()) == Some(false) {
        Err(result["error"]
            .as_str()
            .unwrap_or("tool failed")
            .to_string())
    } else {
        Ok(())
    }
}

fn clean_key(message: &str) -> Result<String, String> {
    let key = message.trim();
    if key.is_empty() {
        return Err("API key must not be empty".to_string());
    }
    Ok(key.to_string())
}

#[async_trait]
impl InternalTool for OnboardingTool {
    fn name(&self) -> &str {
        "onboarding"
    }

    fn tool_definition(&self) -> ToolDefinition {
        tools::tool_definition::<OnboardingParams>(
            self.name(),
            "First-run Warden setup. Greets the user, collects NEAR_API_KEY, \
             stores it encrypted, tests raw_inference, then enables the normal \
             handle-conversation automation.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
        responder: ToolResponder,
    ) -> Result<(), String> {
        let output = tools::execute_typed(args, |params: OnboardingParams| async {
            OnboardingTool::execute(self, params, ctx).await
        })
        .await?;
        responder.respond(output)?;
        Ok(())
    }
}
