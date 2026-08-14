use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rand::seq::SliceRandom;

use crate::{
    automations::{Automation, ToolEffect, UserMessageEffect},
    bus::{Bus, Event},
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
        Automation::OnUserMessageIn {
            name: AUTOMATION_NAME.to_string(),
            conversation: None,
            effect: UserMessageEffect::Tool(ToolEffect {
                tool: "unlock_secrets".to_string(),
                args: serde_json::json!({
                    "message": "{{event.text}}",
                    "message_id": "{{event.message_id}}",
                }),
            }),
        }
    }

    async fn execute(
        &self,
        params: UnlockSecretsParams,
        ctx: &ExecutionContext,
    ) -> Result<(), String> {
        let step = {
            let mut session = self.session.lock().expect("unlock session lock");
            session.handle(&self.layout, &params.message)?
        };
        if step.delete_current_message {
            delete_current_message(
                &self.tools,
                ctx,
                params.message_id.as_deref(),
                "unlock_secrets",
            )
            .await;
        }
        if let Some(message_id) = &step.delete_outbound_message_id {
            delete_outbound_message(&self.tools, ctx, message_id).await?;
        }
        for message in &step.messages {
            send_unlock_message(&self.tools, message, ctx).await?;
        }
        if step.ask_written_down {
            request_written_down_confirmation(&self.tools, ctx).await?;
        }
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
            "Unlock or initialize Ironpaw's encrypted secret store during startup.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ExecutionContext,
        responder: ToolResponder,
    ) {
        responder.respond(
            tools::execute_typed(args, |params: UnlockSecretsParams| async {
                self.execute(params, ctx).await?;
                Ok(serde_json::json!({ "ok": true }))
            })
            .await,
        );
    }
}

enum UnlockSession {
    Locked,
    AwaitingUnlockPhrase,
    AwaitingWrittenDownConfirmation {
        phrase: String,
        checks: Vec<usize>,
        phrase_message_id: String,
    },
    Creating {
        phrase: String,
        checks: Vec<usize>,
        confirmed: usize,
    },
}

impl UnlockSession {
    fn handle(&mut self, layout: &RuntimeLayout, message: &str) -> Result<UnlockStep, String> {
        match self {
            Self::Locked => {
                if EncryptedSecretStore::exists(layout) {
                    *self = Self::AwaitingUnlockPhrase;
                    Ok(UnlockStep {
                        messages: vec![UnlockMessage::plain(
                            "Ironpaw is locked. Enter Ironpaw's 6-word unlock phrase.",
                        )],
                        ask_written_down: false,
                        restart: false,
                        delete_current_message: false,
                        delete_outbound_message_id: None,
                        unlocked: None,
                    })
                } else {
                    Ok(self.start_new_phrase())
                }
            }
            Self::AwaitingUnlockPhrase => unlock_existing(layout, message),
            Self::AwaitingWrittenDownConfirmation {
                phrase,
                checks,
                phrase_message_id,
            } => {
                if message.trim().eq_ignore_ascii_case("yes") {
                    let phrase = phrase.clone();
                    let checks = checks.clone();
                    let phrase_message_id = phrase_message_id.clone();
                    let first_check = checks[0];
                    *self = Self::Creating {
                        phrase,
                        checks,
                        confirmed: 0,
                    };
                    Ok(waiting(format!("Word #{}.", first_check + 1))
                        .delete_outbound_message(phrase_message_id))
                } else {
                    Ok(waiting_for_confirmation(
                        "Select Yes after you have written down the unlock phrase.",
                    ))
                }
            }
            Self::Creating {
                phrase,
                checks,
                confirmed,
            } => {
                let step = verify_new_store(layout, phrase, checks, confirmed, message)?;
                if step.restart {
                    Ok(self.start_new_phrase())
                } else {
                    Ok(step)
                }
            }
        }
    }

    fn start_new_phrase(&mut self) -> UnlockStep {
        let phrase = generate_phrase();
        let checks = phrase_checks(&phrase);
        let phrase_message_id = format!("unlock-phrase-{}", uuid::Uuid::new_v4());
        *self = Self::AwaitingWrittenDownConfirmation {
            phrase: phrase.clone(),
            checks,
            phrase_message_id: phrase_message_id.clone(),
        };
        waiting_for_confirmation_messages(vec![
            UnlockMessage::plain("Write down this 6-word unlock phrase. It will not be stored."),
            UnlockMessage::with_id(phrase, phrase_message_id),
        ])
    }
}

struct UnlockStep {
    messages: Vec<UnlockMessage>,
    ask_written_down: bool,
    restart: bool,
    delete_current_message: bool,
    delete_outbound_message_id: Option<String>,
    unlocked: Option<Arc<EncryptedSecretStore>>,
}

struct UnlockMessage {
    content: String,
    message_id: Option<String>,
}

impl UnlockMessage {
    fn plain(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            message_id: None,
        }
    }

    fn with_id(content: impl Into<String>, message_id: String) -> Self {
        Self {
            content: content.into(),
            message_id: Some(message_id),
        }
    }
}

fn unlock_existing(layout: &RuntimeLayout, message: &str) -> Result<UnlockStep, String> {
    if !phrase_is_valid(message) {
        return Ok(waiting(
            "Ironpaw is locked. Enter Ironpaw's 6-word unlock phrase.",
        ));
    }
    match EncryptedSecretStore::unlock(layout, message) {
        Ok(store) => Ok(unlocked("Secrets unlocked. Starting Ironpaw.", store)),
        Err(_) => Ok(waiting(
            "Ironpaw is locked. Enter Ironpaw's 6-word unlock phrase.",
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
        return Ok(restart());
    }
    *confirmed += 1;
    if let Some(position) = checks.get(*confirmed) {
        return Ok(waiting(format!("Word #{}.", position + 1)));
    }

    let store = EncryptedSecretStore::create(layout, phrase)?;
    Ok(unlocked("Secret store created. Starting Ironpaw.", store))
}

fn phrase_checks(phrase: &str) -> Vec<usize> {
    let mut checks = (0..phrase_words(phrase).len()).collect::<Vec<_>>();
    checks.shuffle(&mut rand::thread_rng());
    checks.truncate(3);
    checks.sort_unstable();
    checks
}

fn waiting(message: impl Into<String>) -> UnlockStep {
    UnlockStep {
        messages: vec![UnlockMessage::plain(message)],
        ask_written_down: false,
        restart: false,
        delete_current_message: true,
        delete_outbound_message_id: None,
        unlocked: None,
    }
}

fn waiting_for_confirmation(message: impl Into<String>) -> UnlockStep {
    waiting_for_confirmation_messages(vec![UnlockMessage::plain(message)])
}

fn waiting_for_confirmation_messages(messages: Vec<UnlockMessage>) -> UnlockStep {
    UnlockStep {
        messages,
        ask_written_down: true,
        restart: false,
        delete_current_message: false,
        delete_outbound_message_id: None,
        unlocked: None,
    }
}

fn restart() -> UnlockStep {
    UnlockStep {
        messages: Vec::new(),
        ask_written_down: false,
        restart: true,
        delete_current_message: true,
        delete_outbound_message_id: None,
        unlocked: None,
    }
}

fn unlocked(message: impl Into<String>, store: Arc<EncryptedSecretStore>) -> UnlockStep {
    UnlockStep {
        messages: vec![UnlockMessage::plain(message)],
        ask_written_down: false,
        restart: false,
        delete_current_message: true,
        delete_outbound_message_id: None,
        unlocked: Some(store),
    }
}

impl UnlockStep {
    fn delete_outbound_message(mut self, message_id: String) -> Self {
        self.delete_current_message = false;
        self.delete_outbound_message_id = Some(message_id);
        self
    }
}

async fn request_written_down_confirmation(
    tools: &InternalTools,
    ctx: &ExecutionContext,
) -> Result<(), String> {
    let result = tools::run_tool(
        tools,
        "request_user_input",
        serde_json::json!({
            "question": "Did you write it down?",
            "options": [{ "label": "Yes", "value": "yes" }],
        }),
        ctx,
    )
    .await;
    result.map(|_| ()).map_err(|error| error.message)
}

async fn send_unlock_message(
    tools: &InternalTools,
    message: &UnlockMessage,
    ctx: &ExecutionContext,
) -> Result<(), String> {
    let Some(message_id) = &message.message_id else {
        return send(tools, &message.content, ctx).await;
    };
    let crate::tools::InvocationScope::Conversation(conversation) = &ctx.invoked_by else {
        return Err("unlock messages require a conversation".to_string());
    };
    let result = tools::run_tool(
        tools,
        "send_message",
        serde_json::json!({
            "channel": conversation.channel,
            "chat_id": conversation.chat_id,
            "thread_id": conversation.thread_id,
            "content": message.content,
            "message_id": message_id,
        }),
        ctx,
    )
    .await;
    result.map(|_| ()).map_err(|error| error.message)
}

async fn delete_outbound_message(
    tools: &InternalTools,
    ctx: &ExecutionContext,
    message_id: &str,
) -> Result<(), String> {
    let result = tools::run_tool(
        tools,
        "delete_message",
        serde_json::json!({ "message_id": message_id }),
        ctx,
    )
    .await;
    result.map(|_| ()).map_err(|error| error.message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> RuntimeLayout {
        RuntimeLayout::from_root(
            std::env::temp_dir().join(format!("ironpaw-unlock-test-{}", uuid::Uuid::new_v4())),
        )
    }

    #[test]
    fn existing_store_preserves_first_message_then_deletes_unlock_phrase() {
        let layout = layout();
        let phrase = "alpha bravo charlie delta echo foxtrot";
        EncryptedSecretStore::create(&layout, phrase).unwrap();
        let mut session = UnlockSession::Locked;

        let first = session.handle(&layout, "hello").unwrap();

        assert!(!first.delete_current_message);
        assert_eq!(
            first.messages[0].content,
            "Ironpaw is locked. Enter Ironpaw's 6-word unlock phrase."
        );
        assert!(matches!(session, UnlockSession::AwaitingUnlockPhrase));

        let unlock = session.handle(&layout, phrase).unwrap();

        assert!(unlock.delete_current_message);
        assert!(unlock.unlocked.is_some());
    }

    #[test]
    fn wrong_word_restarts_with_a_new_phrase() {
        let mut session = UnlockSession::Creating {
            phrase: "alpha bravo charlie delta echo foxtrot".to_string(),
            checks: vec![0],
            confirmed: 0,
        };

        let step = session.handle(&layout(), "not-the-word").unwrap();

        assert_eq!(
            step.messages[0].content,
            "Write down this 6-word unlock phrase. It will not be stored."
        );
        assert_eq!(phrase_words(&step.messages[1].content).len(), 6);
        assert!(step.ask_written_down);
        assert!(matches!(
            session,
            UnlockSession::AwaitingWrittenDownConfirmation { .. }
        ));
    }
}
