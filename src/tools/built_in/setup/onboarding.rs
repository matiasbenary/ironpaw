use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::{
    automations::{Automation, ToolEffect, UserMessageEffect},
    bus::{ConversationKey, Event},
    message::{ToolDefinition, conversation_workspace},
    tools::{
        self, ExecutionContext, InternalTool, ToolResponder,
        setup::{SetupServices, delete_current_message, send},
    },
};

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct OnboardingParams {
    pub conversation: ConversationKey,
    /// The latest user message.
    #[serde(default)]
    pub message: String,
    /// Provider message id for the latest user message, when available.
    #[serde(default)]
    pub message_id: Option<String>,
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
        Automation::OnUserMessageIn {
            name: "onboarding".to_string(),
            conversation: None,
            effect: UserMessageEffect::Tool(ToolEffect {
                tool: "onboarding".to_string(),
                args: serde_json::json!({
                    "conversation": "{{event.conversation}}",
                    "message": "{{event.text}}",
                    "message_id": "{{event.message_id}}",
                }),
            }),
        }
    }

    pub fn start(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = Arc::clone(self);
        let mut events = self.setup.bus.subscribe();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(Event::ApiKeyChangeRequested { conversation }) => {
                        let this = Arc::clone(&this);
                        tokio::spawn(async move {
                            let context =
                                ExecutionContext::new(conversation_workspace(&conversation));
                            if let Err(error) =
                                this.start_api_key_change(&conversation, &context).await
                            {
                                eprintln!("could not start API-key change: {error}");
                            }
                        });
                    }
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }

    async fn execute(
        &self,
        params: OnboardingParams,
        ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, String> {
        let stage = *self.stage.lock().expect("onboarding stage lock");
        match stage {
            Stage::Hello => {
                self.set_stage(Stage::NearKey {
                    flow: KeyFlow::FirstRun,
                });
                send(
                    &self.setup.tools,
                    &params.conversation,
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
                    &params.conversation,
                    "Thanks, let me redact that for you.",
                    ctx,
                )
                .await?;
                self.setup.secrets.set("NEAR_API_KEY", &key)?;
                delete_current_message(
                    &self.setup.tools,
                    &params.conversation,
                    ctx,
                    params.message_id.as_deref(),
                    "onboarding",
                )
                .await;
                send(
                    &self.setup.tools,
                    &params.conversation,
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
                        &params.conversation,
                        &format!(
                            "The NEAR API key did not pass a basic inference test: {err}. Paste your NEAR API key again."
                        ),
                        ctx,
                    )
                    .await?;
                    return Ok(serde_json::json!({ "verified": false, "error": err }));
                }
                self.finish(&params.conversation, ctx, flow).await?;
            }
        }
        Ok(serde_json::json!({ "ok": true }))
    }

    async fn test_near(&self, ctx: &ExecutionContext) -> Result<(), String> {
        let result = tools::execute_tool(
            &self.setup.tools.registry,
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
        conversation: &ConversationKey,
        ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, String> {
        if !matches!(
            *self.stage.lock().expect("onboarding stage lock"),
            Stage::Hello
        ) {
            return Ok(serde_json::json!({ "waiting_for_key": true }));
        }
        self.setup
            .automations
            .add_runtime(Automation::OnUserMessageIn {
                name: API_KEY_CHANGE_SESSION_AUTOMATION.to_string(),
                conversation: Some(conversation.clone()),
                effect: UserMessageEffect::Tool(ToolEffect {
                    tool: "onboarding".to_string(),
                    args: serde_json::json!({
                        "conversation": "{{event.conversation}}",
                        "message": "{{event.text}}",
                        "message_id": "{{event.message_id}}",
                    }),
                }),
            })?;
        self.set_stage(Stage::NearKey {
            flow: KeyFlow::Change,
        });
        send(
            &self.setup.tools,
            conversation,
            "Paste a new NEAR API key.",
            ctx,
        )
        .await?;
        Ok(serde_json::json!({ "waiting_for_key": true }))
    }

    async fn finish(
        &self,
        conversation: &ConversationKey,
        ctx: &ExecutionContext,
        flow: KeyFlow,
    ) -> Result<(), String> {
        let message = match flow {
            KeyFlow::FirstRun => {
                self.setup.automations.remove_runtime("onboarding");
                "Ironpaw is set up. From now on, I will answer your messages normally."
            }
            KeyFlow::Change => {
                self.setup
                    .automations
                    .remove_runtime(API_KEY_CHANGE_SESSION_AUTOMATION);
                "API key changed."
            }
        };
        send(&self.setup.tools, conversation, message, ctx).await?;
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
