//! Host-side user approval for tool actions that move money or run privileged commands.
//!
//! The tool never sees the decision: the host parks the exact request, asks the user with
//! buttons, and only lets an identical request through once after the *user's own* message
//! (`approve:<code>`) arrives on the bus. A model cannot produce that message.
//!
//! One request waits per conversation at a time, so the user never faces several cards
//! whose order matters. State is written to disk so a restart does not drop a pending
//! question or an approval the user already gave.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::bus::{Bus, ConversationKey, Event, UserInputOption, UserInputRequest};

const TTL_SECS: u64 = 600;

pub enum Check {
    Approved,
    /// Waiting for the user. `asked` is false when this request was already put to them.
    Pending {
        asked: bool,
    },
    /// A different request is still waiting for the user in this conversation.
    Busy,
}

#[derive(Clone, Serialize, Deserialize)]
struct Pending {
    fingerprint: String,
    conversation: ConversationKey,
    created: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    pending: HashMap<String, Pending>,
    approved: HashMap<String, u64>,
    /// Where to ask when a run has no conversation (e.g. a scheduled automation).
    last_conversation: Option<ConversationKey>,
}

#[derive(Default)]
pub struct Approvals {
    state: Mutex<State>,
    bus: OnceLock<Bus>,
    path: OnceLock<PathBuf>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

impl Approvals {
    pub fn global() -> &'static Approvals {
        static GLOBAL: OnceLock<Approvals> = OnceLock::new();
        GLOBAL.get_or_init(Approvals::default)
    }

    /// Connects the store to the bus and reloads what a previous run saved at `path`.
    pub fn attach(&self, bus: Bus, path: PathBuf) {
        let _ = self.bus.set(bus);
        if let Some(saved) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<State>(&text).ok())
        {
            let mut state = self.state.lock().expect("approvals lock");
            *state = saved;
            state.prune();
        }
        let _ = self.path.set(path);
    }

    /// Remembers the owner's latest conversation so unattended runs can ask there.
    pub fn note_user_conversation(&self, conversation: &ConversationKey) {
        let mut state = self.state.lock().expect("approvals lock");
        if state.last_conversation.as_ref() != Some(conversation) {
            state.last_conversation = Some(conversation.clone());
            self.save(&state);
        }
    }

    /// Lets the request through once the user approved it; otherwise asks them and fails
    /// with the message the tool should report. `conversation` is the run's own, if any.
    pub fn require(
        &self,
        conversation: Option<&ConversationKey>,
        fingerprint: &str,
        summary: &str,
    ) -> Result<(), String> {
        let conversation = match conversation {
            Some(conversation) => conversation.clone(),
            None => self
                .state
                .lock()
                .expect("approvals lock")
                .last_conversation
                .clone()
                .ok_or(
                    "this action needs the user's approval but there is no conversation to ask",
                )?,
        };
        match self.check(fingerprint, &conversation, summary)? {
            Check::Approved => Ok(()),
            Check::Pending { .. } => Err(
                "APPROVAL_REQUIRED: nothing was sent. The user was asked to approve this exact \
                 action with buttons. End your turn and tell them to approve or reject. Only after \
                 they approve, repeat the identical call once; never retry before."
                    .to_string(),
            ),
            Check::Busy => Err(
                "APPROVAL_REQUIRED: nothing was sent. Another action is still waiting for the \
                 user's approval. Do not make further money or privileged calls until they answer \
                 it; end your turn."
                    .to_string(),
            ),
        }
    }

    fn check(
        &self,
        fingerprint: &str,
        conversation: &ConversationKey,
        summary: &str,
    ) -> Result<Check, String> {
        let bus = self
            .bus
            .get()
            .ok_or_else(|| "user approval is not available in this runtime".to_string())?;
        let mut state = self.state.lock().expect("approvals lock");
        state.prune();
        if state.approved.remove(fingerprint).is_some() {
            self.save(&state);
            return Ok(Check::Approved);
        }
        let in_conversation = |pending: &&Pending| &pending.conversation == conversation;
        if state
            .pending
            .values()
            .filter(in_conversation)
            .any(|pending| pending.fingerprint == fingerprint)
        {
            return Ok(Check::Pending { asked: false });
        }
        if state
            .pending
            .values()
            .any(|pending| in_conversation(&pending))
        {
            return Ok(Check::Busy);
        }
        let code = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        state.pending.insert(
            code.clone(),
            Pending {
                fingerprint: fingerprint.to_string(),
                conversation: conversation.clone(),
                created: now(),
            },
        );
        self.save(&state);
        bus.publish(Event::UserInputRequested {
            conversation: conversation.clone(),
            request: UserInputRequest {
                question: summary.to_string(),
                options: vec![
                    UserInputOption {
                        label: "Approve".to_string(),
                        value: format!("approve:{code}"),
                    },
                    UserInputOption {
                        label: "Reject".to_string(),
                        value: format!("reject:{code}"),
                    },
                ],
            },
        });
        Ok(Check::Pending { asked: true })
    }

    /// Applies a user's `approve:<code>` / `reject:<code>` answer. Returns whether it matched.
    pub fn resolve(&self, conversation: &ConversationKey, text: &str) -> bool {
        let text = text.trim();
        let (approve, code) = if let Some(code) = text.strip_prefix("approve:") {
            (true, code)
        } else if let Some(code) = text.strip_prefix("reject:") {
            (false, code)
        } else {
            return false;
        };
        let mut state = self.state.lock().expect("approvals lock");
        state.prune();
        if state
            .pending
            .get(code)
            .is_none_or(|pending| &pending.conversation != conversation)
        {
            return false;
        }
        let pending = state.pending.remove(code).expect("checked above");
        if approve {
            state.approved.insert(pending.fingerprint, now());
        }
        self.save(&state);
        true
    }

    fn save(&self, state: &State) {
        let Some(path) = self.path.get() else {
            return;
        };
        let written = serde_json::to_string(state)
            .map_err(|err| err.to_string())
            .and_then(|text| std::fs::write(path, text).map_err(|err| err.to_string()));
        if let Err(error) = written {
            eprintln!("could not save approvals: {error}");
        }
    }
}

impl State {
    fn prune(&mut self) {
        let cutoff = now().saturating_sub(TTL_SECS);
        self.pending.retain(|_, pending| pending.created >= cutoff);
        self.approved.retain(|_, at| *at >= cutoff);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(chat: &str) -> ConversationKey {
        ConversationKey {
            channel: "local".to_string(),
            chat_id: chat.to_string(),
            thread_id: None,
        }
    }

    fn setup(path: Option<PathBuf>) -> (Approvals, tokio::sync::broadcast::Receiver<Event>) {
        let bus = Bus::new(8);
        let events = bus.subscribe();
        let approvals = Approvals::default();
        approvals.attach(
            bus,
            path.unwrap_or_else(|| {
                std::env::temp_dir().join(format!("approvals-{}.json", uuid::Uuid::new_v4()))
            }),
        );
        (approvals, events)
    }

    fn code_of(events: &mut tokio::sync::broadcast::Receiver<Event>) -> String {
        let Event::UserInputRequested { request, .. } = events.try_recv().unwrap() else {
            panic!("expected a question");
        };
        request.options[0].value.clone()
    }

    fn is(result: Result<Check, String>, expected: &str) -> bool {
        match (result, expected) {
            (Ok(Check::Approved), "approved") => true,
            (Ok(Check::Pending { asked: true }), "asked") => true,
            (Ok(Check::Pending { asked: false }), "waiting") => true,
            (Ok(Check::Busy), "busy") => true,
            _ => false,
        }
    }

    #[test]
    fn only_the_users_approval_lets_one_identical_request_through() {
        let (approvals, mut events) = setup(None);
        let (mine, other) = (key("1"), key("2"));

        assert!(is(approvals.check("send 1", &mine, "ok?"), "asked"));
        let approve = code_of(&mut events);
        assert!(is(approvals.check("send 1", &mine, "ok?"), "waiting"));
        assert!(events.try_recv().is_err(), "asked twice");

        assert!(!approvals.resolve(&other, &approve), "other chat");
        assert!(!approvals.resolve(&mine, "approve:nope"));
        assert!(approvals.resolve(&mine, &approve));
        assert!(is(approvals.check("send 1", &mine, "ok?"), "approved"));
        // Single use.
        assert!(is(approvals.check("send 1", &mine, "ok?"), "asked"));
    }

    #[test]
    fn rejecting_does_not_approve() {
        let (approvals, mut events) = setup(None);
        let mine = key("1");
        approvals.check("send", &mine, "ok?").unwrap();
        let reject = code_of(&mut events).replace("approve:", "reject:");
        assert!(approvals.resolve(&mine, &reject));
        assert!(is(approvals.check("send", &mine, "ok?"), "asked"));
    }

    #[test]
    fn a_second_request_waits_for_the_first_answer() {
        let (approvals, mut events) = setup(None);
        let (mine, other) = (key("1"), key("2"));

        assert!(is(approvals.check("wrap", &mine, "ok?"), "asked"));
        let approve = code_of(&mut events);
        assert!(is(approvals.check("deposit", &mine, "ok?"), "busy"));
        assert!(events.try_recv().is_err(), "no second card");
        // Other conversations are independent.
        assert!(is(approvals.check("deposit", &other, "ok?"), "asked"));

        assert!(approvals.resolve(&mine, &approve));
        assert!(is(approvals.check("deposit", &mine, "ok?"), "asked"));
    }

    #[test]
    fn unattended_runs_ask_in_the_last_user_conversation() {
        let (approvals, mut events) = setup(None);
        assert!(approvals.require(None, "send", "ok?").is_err());
        assert!(events.try_recv().is_err(), "nowhere to ask");

        let mine = key("1");
        approvals.note_user_conversation(&mine);
        let error = approvals.require(None, "send", "ok?").unwrap_err();
        assert!(error.starts_with("APPROVAL_REQUIRED"), "{error}");
        let Event::UserInputRequested { conversation, .. } = events.try_recv().unwrap() else {
            panic!("expected a question");
        };
        assert_eq!(conversation, mine);
    }

    #[test]
    fn pending_and_approved_requests_survive_a_restart() {
        let path = std::env::temp_dir().join(format!("approvals-{}.json", uuid::Uuid::new_v4()));
        let mine = key("1");
        let (before, mut events) = setup(Some(path.clone()));
        before.check("wrap", &mine, "ok?").unwrap();
        let approve = code_of(&mut events);
        before.resolve(&mine, &approve);
        before.check("deposit", &mine, "ok?").unwrap();
        let deposit = code_of(&mut events);

        let (after, _events) = setup(Some(path));
        assert!(is(after.check("wrap", &mine, "ok?"), "approved"));
        assert!(after.resolve(&mine, &deposit), "pending question kept");
        assert!(is(after.check("deposit", &mine, "ok?"), "approved"));
    }
}
