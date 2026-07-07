use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rand::seq::SliceRandom;

use crate::{
    automations::{Automation, AutomationEffect, Trigger},
    bus::{Bus, Event, SenderKind},
    config::RuntimeLayout,
    message::ToolDefinition,
    secrets::{EncryptedSecretStore, Secrets, generate_phrase, phrase_is_valid, phrase_words},
    tools::{self, ExecutionContext, InternalTool, InternalTools, ToolResponder},
};

use super::setup::{delete_current_message, send};

pub const AUTOMATION_NAME: &str = "unlock-secrets";

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct UnlockSecretsParams {
    /// The latest user message.
    pub message: String,
    /// Provider message id for the latest user message, when available.
    #[serde(default)]
    pub message_id: Option<String>,
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
            session: Mutex::new(UnlockSession::Locked),
            layout,
            secrets,
            tools,
        }
    }

    pub fn automation() -> Automation {
        Automation {
            name: AUTOMATION_NAME.to_string(),
            trigger: Trigger::MessageIn {
                sender: SenderKind::User,
                conversation: None,
            },
            effect: AutomationEffect::Tool {
                tool: "unlock_secrets".to_string(),
                args: serde_json::json!({
                    "message": "{{event.text}}",
                    "message_id": "{{event.message_id}}",
                }),
            },
        }
    }

    async fn execute(
        &self,
        params: UnlockSecretsParams,
        ctx: &ExecutionContext,
    ) -> Result<(), String> {
        let (message, unlocked) = {
            let mut session = self.session.lock().expect("unlock session lock");
            session.handle(&self.layout, &params.message)?
        };
        send(&self.tools, &message, ctx).await?;
        delete_current_message(
            &self.tools,
            ctx,
            params.message_id.as_deref(),
            "unlock_secrets",
        )
        .await;
        if let Some(store) = unlocked {
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
        ctx: &ExecutionContext,
        responder: ToolResponder,
    ) -> Result<(), String> {
        let output = tools::execute_typed(args, |params: UnlockSecretsParams| async {
            self.execute(params, ctx).await?;
            Ok(serde_json::json!({ "ok": true }))
        })
        .await?;
        responder.respond(output)?;
        Ok(())
    }
}

enum UnlockSession {
    Locked,
    Creating {
        phrase: String,
        checks: Vec<usize>,
        confirmed: usize,
    },
}

impl UnlockSession {
    fn handle(&mut self, layout: &RuntimeLayout, message: &str) -> Result<UnlockStep, String> {
        if EncryptedSecretStore::exists(layout) {
            return unlock_existing(layout, message);
        }

        match self {
            Self::Locked => {
                let phrase = generate_phrase();
                let checks = phrase_checks(&phrase);
                let prompt = phrase_prompt(&phrase, checks[0]);
                *self = Self::Creating {
                    phrase,
                    checks,
                    confirmed: 0,
                };
                Ok(waiting(prompt))
            }
            Self::Creating {
                phrase,
                checks,
                confirmed,
            } => verify_new_store(layout, phrase, checks, confirmed, message),
        }
    }
}

type UnlockStep = (String, Option<Arc<EncryptedSecretStore>>);

fn unlock_existing(layout: &RuntimeLayout, message: &str) -> Result<UnlockStep, String> {
    if !phrase_is_valid(message) {
        return Ok(waiting(
            "Warden is locked. Enter Warden's 6-word unlock phrase.",
        ));
    }
    match EncryptedSecretStore::unlock(layout, message) {
        Ok(store) => Ok(unlocked("Secrets unlocked. Starting Warden.", store)),
        Err(_) => Ok(waiting(
            "Warden is locked. Enter Warden's 6-word unlock phrase.",
        )),
    }
}

fn verify_new_store(
    layout: &RuntimeLayout,
    phrase: &str,
    checks: &[usize],
    confirmed: &mut usize,
    message: &str,
) -> Result<UnlockStep, String> {
    let words = phrase_words(phrase);
    let position = checks[*confirmed];
    if message.trim().to_ascii_lowercase() != words[position] {
        return Ok(waiting(format!(
            "That did not match word #{}. Try again.",
            position + 1
        )));
    }
    *confirmed += 1;
    if let Some(position) = checks.get(*confirmed) {
        return Ok(waiting(format!("Word #{}.", position + 1)));
    }

    let store = EncryptedSecretStore::create(layout, phrase)?;
    Ok(unlocked("Secret store created. Starting Warden.", store))
}

fn phrase_checks(phrase: &str) -> Vec<usize> {
    let mut checks = (0..phrase_words(phrase).len()).collect::<Vec<_>>();
    checks.shuffle(&mut rand::thread_rng());
    checks.truncate(3);
    checks.sort_unstable();
    checks
}

fn phrase_prompt(phrase: &str, first_check: usize) -> String {
    format!(
        "Warden needs to create an encrypted secret store.\n\nWrite down this 6-word unlock phrase. It will not be stored:\n\n{phrase}\n\nReply with word #{}.",
        first_check + 1
    )
}

fn waiting(message: impl Into<String>) -> UnlockStep {
    (message.into(), None)
}

fn unlocked(message: impl Into<String>, store: Arc<EncryptedSecretStore>) -> UnlockStep {
    (message.into(), Some(store))
}
