use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rand::seq::SliceRandom;

use crate::{
    automations::{Automation, AutomationEffect, Trigger},
    bus::{Bus, Event, Sender},
    config::RuntimeLayout,
    message::ToolDefinition,
    secrets::{EncryptedSecretStore, Secrets, generate_phrase, phrase_is_valid, phrase_words},
    tools::{self, InternalTool, InternalTools, ToolContext, ToolOutcome},
};

pub const AUTOMATION_NAME: &str = "unlock-secrets";

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct UnlockSecretsParams {
    /// The latest user message.
    pub message: String,
}

pub struct UnlockSecretsTool {
    bus: Bus,
    layout: RuntimeLayout,
    secrets: Secrets,
    tools: Arc<InternalTools>,
    session: Mutex<UnlockSession>,
}

impl UnlockSecretsTool {
    pub fn new(
        bus: Bus,
        layout: RuntimeLayout,
        secrets: Secrets,
        tools: Arc<InternalTools>,
    ) -> Self {
        Self {
            bus,
            session: Mutex::new(UnlockSession::new(&layout)),
            layout,
            secrets,
            tools,
        }
    }

    pub fn automation() -> Automation {
        Automation {
            name: AUTOMATION_NAME.to_string(),
            trigger: Trigger::MessageIn {
                sender: Some(Sender::User),
                channel: None,
            },
            effect: AutomationEffect::Tool {
                tool: "unlock_secrets".to_string(),
                args: serde_json::json!({
                    "message": "{{event.text}}",
                }),
            },
        }
    }

    async fn execute(&self, params: UnlockSecretsParams, ctx: &ToolContext) -> Result<(), String> {
        let step = {
            let mut session = self.session.lock().expect("unlock session lock");
            session.handle(&self.layout, &params.message)?
        };
        send(&self.tools, &step.message, ctx).await?;
        delete_current_message(&self.tools, ctx).await;
        if let Some(store) = step.unlocked {
            self.secrets.unlock(store);
            self.bus.publish(Event::SecretsUnlocked);
        }
        Ok(())
    }
}

#[async_trait]
impl InternalTool for UnlockSecretsTool {
    fn name(&self) -> &str {
        "unlock_secrets"
    }

    fn tool_definition(&self) -> ToolDefinition {
        tools::tool_definition::<UnlockSecretsParams>(
            self.name(),
            "Unlock or initialize Warden's encrypted secret store during startup.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutcome, String> {
        let output = tools::execute_typed(args, |params: UnlockSecretsParams| async {
            self.execute(params, ctx).await?;
            Ok(serde_json::json!({ "ok": true }))
        })
        .await?;
        Ok(ToolOutcome::Finished(output))
    }
}

enum UnlockSession {
    Existing,
    New {
        phrase: String,
        words: Vec<String>,
        positions: Vec<usize>,
        next_check: usize,
    },
}

impl UnlockSession {
    fn new(layout: &RuntimeLayout) -> Self {
        if EncryptedSecretStore::exists(layout) {
            Self::Existing
        } else {
            let phrase = generate_phrase();
            let words = phrase_words(&phrase);
            let mut positions = (0..words.len()).collect::<Vec<_>>();
            positions.shuffle(&mut rand::thread_rng());
            positions.truncate(3);
            positions.sort_unstable();
            Self::New {
                phrase,
                words,
                positions,
                next_check: 0,
            }
        }
    }

    fn handle(&mut self, layout: &RuntimeLayout, message: &str) -> Result<UnlockStep, String> {
        match self {
            Self::Existing => unlock_existing(layout, message),
            Self::New {
                phrase,
                words,
                positions,
                next_check,
            } => initialize_or_verify_new_store(
                layout, phrase, words, positions, next_check, message,
            ),
        }
    }
}

struct UnlockStep {
    message: String,
    unlocked: Option<Arc<EncryptedSecretStore>>,
}

fn unlock_existing(layout: &RuntimeLayout, message: &str) -> Result<UnlockStep, String> {
    if !phrase_is_valid(message) {
        return Ok(UnlockStep {
            message: "Enter Warden's 6-word unlock phrase.".to_string(),
            unlocked: None,
        });
    }
    match EncryptedSecretStore::unlock(layout, message) {
        Ok(store) => Ok(UnlockStep {
            message: "Secrets unlocked. Starting Warden.".to_string(),
            unlocked: Some(store),
        }),
        Err(_) => Ok(UnlockStep {
            message: "That unlock phrase did not work. Try again.".to_string(),
            unlocked: None,
        }),
    }
}

fn initialize_or_verify_new_store(
    layout: &RuntimeLayout,
    phrase: &str,
    words: &[String],
    positions: &[usize],
    next_check: &mut usize,
    message: &str,
) -> Result<UnlockStep, String> {
    if *next_check == 0 {
        *next_check = 1;
        return Ok(UnlockStep {
            message: format!(
                "Warden needs to create an encrypted secret store.\n\nWrite down this 6-word unlock phrase. It will not be stored:\n\n{phrase}\n\nReply with word #{}.",
                positions[0] + 1
            ),
            unlocked: None,
        });
    }

    let position = positions[*next_check - 1];
    if message.trim().to_ascii_lowercase() != words[position] {
        return Err("unlock phrase verification failed".to_string());
    }
    if let Some(position) = positions.get(*next_check) {
        *next_check += 1;
        return Ok(UnlockStep {
            message: format!("Word #{}.", position + 1),
            unlocked: None,
        });
    }

    let store = EncryptedSecretStore::create(layout, phrase)?;
    Ok(UnlockStep {
        message: "Secret store created. Starting Warden.".to_string(),
        unlocked: Some(store),
    })
}

async fn send(tools: &InternalTools, content: &str, ctx: &ToolContext) -> Result<(), String> {
    let result = tools::run_tool(
        tools,
        "send_message",
        serde_json::json!({ "content": content }),
        ctx,
    )
    .await;
    if result.get("ok").and_then(|ok| ok.as_bool()) == Some(false) {
        return Err(result["error"]
            .as_str()
            .unwrap_or("send failed")
            .to_string());
    }
    Ok(())
}

async fn delete_current_message(tools: &InternalTools, ctx: &ToolContext) {
    let result = tools::run_tool(tools, "delete_message", serde_json::json!({}), ctx).await;
    if result.get("ok").and_then(|ok| ok.as_bool()) == Some(false) {
        eprintln!(
            "unlock_secrets could not delete inbound unlock message: {}",
            result["error"].as_str().unwrap_or("unknown error")
        );
    }
}
