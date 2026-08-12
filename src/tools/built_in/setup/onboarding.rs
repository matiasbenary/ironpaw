use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::{
    automations::{Automation, AutomationEffect, Automations, Trigger},
    bus::{AgentRequest, Event, InvocationScope, SenderKind},
    history::{HistoryEntry, MessageMetadata},
    message::{Message, ToolDefinition},
    tools::{
        self, ExecutionContext, InternalTool, ToolResponder,
        setup::{SetupServices, delete_current_message, pause_conversation, request_choice, send},
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
    /// Start API-key recovery after an inference request was rejected.
    #[serde(default)]
    pub recover: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Hello,
    RenewalChoice,
    NearKey { resume_conversation: bool },
}

const API_KEY_ERROR_AUTOMATION: &str = "recover-api-key";
const API_KEY_RENEWAL_AUTOMATION: &str = "renew-api-key";

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

    pub fn api_key_recovery_automation() -> Automation {
        Automation {
            name: API_KEY_ERROR_AUTOMATION.to_string(),
            trigger: Trigger::ApiKeyError,
            effect: AutomationEffect::Tool {
                tool: "onboarding".to_string(),
                args: serde_json::json!({ "recover": true }),
            },
        }
    }

    async fn execute(
        &self,
        params: OnboardingParams,
        ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, String> {
        if params.recover {
            return self.start_api_key_recovery(ctx).await;
        }
        let stage = *self.stage.lock().expect("onboarding stage lock");
        match stage {
            Stage::Hello => {
                self.set_stage(Stage::NearKey {
                    resume_conversation: false,
                });
                send(
                    &self.setup.tools,
                    "Welcome to Ironpaw. Let's set up the API key I need. Paste your NEAR API key.",
                    ctx,
                )
                .await?;
            }
            Stage::RenewalChoice => {
                if is_yes(&params.message) {
                    self.set_stage(Stage::NearKey {
                        resume_conversation: true,
                    });
                    send(
                        &self.setup.tools,
                        "Paste a new NEAR API key. Once it works, I will retry your request.",
                        ctx,
                    )
                    .await?;
                } else {
                    self.setup
                        .automations
                        .remove_transient(API_KEY_RENEWAL_AUTOMATION);
                    self.setup
                        .automations
                        .add(Automations::handle_conversation())?;
                    self.set_stage(Stage::Hello);
                    send(
                        &self.setup.tools,
                        "Keeping the current NEAR API key. The failed request was not retried.",
                        ctx,
                    )
                    .await?;
                }
            }
            Stage::NearKey {
                resume_conversation,
            } => {
                let key = clean_key(&params.message)?;
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
                    self.setup.secrets.remove("NEAR_API_KEY")?;
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
                self.finish(ctx, resume_conversation).await?;
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

    async fn start_api_key_recovery(
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
            return Err("API-key recovery requires a conversation".to_string());
        };
        pause_conversation(&self.setup.automations)?;
        self.setup.automations.add_transient(Automation {
            name: API_KEY_RENEWAL_AUTOMATION.to_string(),
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
        self.set_stage(Stage::RenewalChoice);
        request_choice(
            &self.setup.tools,
            "My NEAR API key failed. Do you want to replace it? A validated replacement will retry your request.",
            &[("Replace key", "yes"), ("Keep key", "no")],
            ctx,
        )
        .await?;
        Ok(serde_json::json!({ "waiting_for_key": true }))
    }

    async fn finish(
        &self,
        ctx: &ExecutionContext,
        resume_conversation: bool,
    ) -> Result<(), String> {
        self.setup
            .automations
            .remove_transient(if resume_conversation {
                API_KEY_RENEWAL_AUTOMATION
            } else {
                "onboarding"
            });
        self.setup
            .automations
            .add(Automations::handle_conversation())?;
        let message = if resume_conversation {
            "API key refreshed. Resuming your request."
        } else {
            "Ironpaw is set up. From now on, I will answer your messages normally."
        };
        send(&self.setup.tools, message, ctx).await?;
        if resume_conversation {
            self.resume_conversation(ctx, message)?;
        }
        self.set_stage(Stage::Hello);
        Ok(())
    }

    fn resume_conversation(&self, ctx: &ExecutionContext, status: &str) -> Result<(), String> {
        let InvocationScope::Conversation(conversation) = &ctx.invoked_by else {
            return Err("API-key recovery requires a conversation".to_string());
        };
        self.setup.tools.history.append(
            conversation,
            &[HistoryEntry {
                message: Message::Assistant {
                    text: Some(status.to_string()),
                    attachments: Vec::new(),
                    tool_calls: Vec::new(),
                },
                metadata: MessageMetadata::new(conversation.clone(), None),
            }],
        )?;
        self.setup.bus.publish(Event::AgentRequest {
            execution_id: uuid::Uuid::new_v4().to_string(),
            request: AgentRequest::ContinueConversation {
                conversation: conversation.clone(),
                context: ctx.clone(),
            },
        });
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

fn is_yes(message: &str) -> bool {
    matches!(message.trim().to_ascii_lowercase().as_str(), "yes" | "y")
}

#[async_trait]
impl InternalTool for OnboardingTool {
    fn name(&self) -> &str {
        "onboarding"
    }

    fn tool_definition(&self) -> ToolDefinition {
        tools::tool_definition::<OnboardingParams>(
            self.name(),
            "First-run Ironpaw setup. Greets the user, collects NEAR_API_KEY, \
             stores it encrypted, tests raw_inference, then enables the normal \
             handle-conversation automation.",
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
