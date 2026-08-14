use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::{
    automations::{Automation, AutomationEffect, Automations, Trigger},
    bus::{InvocationScope, SenderKind},
    message::ToolDefinition,
    tools::{
        self, ExecutionContext, InternalTool, ToolResponder,
        setup::{SetupServices, delete_current_message, pause_conversation, send},
    },
};

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct OnboardingParams {
    /// The latest user message.
    #[serde(default)]
    pub message: String,
    /// Provider message id for the latest user message, when available.
    #[serde(default)]
    pub message_id: Option<String>,
    /// Start an owner-requested API-key change.
    #[serde(default)]
    pub change_api_key: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Hello,
    NearKey { flow: KeyFlow },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyFlow {
    FirstRun,
    Change,
}

const API_KEY_CHANGE_REQUEST_AUTOMATION: &str = "request-api-key-change";
const API_KEY_CHANGE_SESSION_AUTOMATION: &str = "change-api-key";

pub struct OnboardingTool {
    stage: Mutex<Stage>,
    setup: Arc<SetupServices>,
}

impl OnboardingTool {
    pub fn new(setup: Arc<SetupServices>) -> Self {
        Self {
            stage: Mutex::new(Stage::Hello),
            setup,
        }
    }

    pub fn automation() -> Automation {
        Automation {
            name: "onboarding".to_string(),
            trigger: Trigger::MessageIn {
                sender: SenderKind::User,
                conversation: None,
            },
            effect: AutomationEffect::Tool {
                tool: "onboarding".to_string(),
                args: serde_json::json!({
                    "message": "{{event.text}}",
                    "message_id": "{{event.message_id}}",
                }),
            },
        }
    }

    pub fn api_key_change_automation() -> Automation {
        Automation {
            name: API_KEY_CHANGE_REQUEST_AUTOMATION.to_string(),
            trigger: Trigger::ApiKeyChangeRequested,
            effect: AutomationEffect::Tool {
                tool: "onboarding".to_string(),
                args: serde_json::json!({ "change_api_key": true }),
            },
        }
    }

    async fn execute(
        &self,
        params: OnboardingParams,
        ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, String> {
        if params.change_api_key {
            return self.start_api_key_change(ctx).await;
        }
        let stage = *self.stage.lock().expect("onboarding stage lock");
        match stage {
            Stage::Hello => {
                self.set_stage(Stage::NearKey {
                    flow: KeyFlow::FirstRun,
                });
                send(
                    &self.setup.tools,
                    "Welcome to Ironpaw. Let's set up the API key I need. Paste your NEAR API key.",
                    ctx,
                )
                .await?;
            }
            Stage::NearKey { flow } => {
                let key = clean_key(&params.message)?;
                let previous_key = self.setup.secrets.get("NEAR_API_KEY");
                send(
                    &self.setup.tools,
                    "Thanks, let me redact that for you.",
                    ctx,
                )
                .await?;
                self.setup.secrets.set("NEAR_API_KEY", &key)?;
                delete_current_message(
                    &self.setup.tools,
                    ctx,
                    params.message_id.as_deref(),
                    "onboarding",
                )
                .await;
                send(
                    &self.setup.tools,
                    "Thanks, let me test the key to make sure it works.",
                    ctx,
                )
                .await?;
                if let Err(err) = self.test_near(ctx).await {
                    match previous_key {
                        Some(previous_key) => {
                            self.setup.secrets.set("NEAR_API_KEY", &previous_key)?
                        }
                        None => self.setup.secrets.remove("NEAR_API_KEY")?,
                    }
                    send(
                        &self.setup.tools,
                        &format!(
                            "The NEAR API key did not pass a basic inference test: {err}. Paste your NEAR API key again."
                        ),
                        ctx,
                    )
                    .await?;
                    return Ok(serde_json::json!({ "verified": false, "error": err }));
                }
                self.finish(ctx, flow).await?;
            }
        }
        Ok(serde_json::json!({ "ok": true }))
    }

    async fn test_near(&self, ctx: &ExecutionContext) -> Result<(), String> {
        let result = tools::run_tool(
            &self.setup.tools,
            "raw_inference",
            serde_json::json!({
                "model": self.setup.tools.inference.provider().text_model.clone(),
                "messages": [
                    { "User": { "text": "Reply with exactly OK.", "attachments": [] } }
                ]
            }),
            ctx,
        )
        .await;
        result.map(|_| ()).map_err(|error| error.message)
    }

    async fn start_api_key_change(
        &self,
        ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, String> {
        if !matches!(
            *self.stage.lock().expect("onboarding stage lock"),
            Stage::Hello
        ) {
            return Ok(serde_json::json!({ "waiting_for_key": true }));
        }
        let InvocationScope::Conversation(conversation) = &ctx.invoked_by else {
            return Err("API-key change requires a conversation".to_string());
        };
        pause_conversation(&self.setup.automations)?;
        self.setup.automations.add_transient(Automation {
            name: API_KEY_CHANGE_SESSION_AUTOMATION.to_string(),
            trigger: Trigger::MessageIn {
                sender: SenderKind::User,
                conversation: Some(conversation.clone()),
            },
            effect: AutomationEffect::Tool {
                tool: "onboarding".to_string(),
                args: serde_json::json!({
                    "message": "{{event.text}}",
                    "message_id": "{{event.message_id}}",
                }),
            },
        })?;
        self.set_stage(Stage::NearKey {
            flow: KeyFlow::Change,
        });
        send(&self.setup.tools, "Paste a new NEAR API key.", ctx).await?;
        Ok(serde_json::json!({ "waiting_for_key": true }))
    }

    async fn finish(&self, ctx: &ExecutionContext, flow: KeyFlow) -> Result<(), String> {
        self.setup.automations.remove_transient(match flow {
            KeyFlow::FirstRun => "onboarding",
            KeyFlow::Change => API_KEY_CHANGE_SESSION_AUTOMATION,
        });
        self.setup
            .automations
            .add(Automations::handle_conversation())?;
        let message = match flow {
            KeyFlow::FirstRun => {
                "Ironpaw is set up. From now on, I will answer your messages normally."
            }
            KeyFlow::Change => "API key changed.",
        };
        send(&self.setup.tools, message, ctx).await?;
        self.set_stage(Stage::Hello);
        Ok(())
    }

    fn set_stage(&self, stage: Stage) {
        *self.stage.lock().expect("onboarding stage lock") = stage;
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
            "First-run setup and owner-requested API-key changes. Collects NEAR_API_KEY, stores \
             it encrypted, tests raw_inference, then enables normal conversation handling.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ExecutionContext,
        responder: ToolResponder,
    ) {
        responder.respond(
            tools::execute_typed(args, |params: OnboardingParams| async {
                OnboardingTool::execute(self, params, ctx).await
            })
            .await,
        );
    }
}
