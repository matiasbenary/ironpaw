use std::{
    str::FromStr,
    sync::{Arc, RwLock},
};

use crate::{
    bus::{Bus, Event, Sender},
    tools::{self, InternalTools, ToolContext},
};

mod store;

pub use store::SqliteAutomations;

/// An automation is a trigger plus a tool call. The stored args may contain
/// `{{event.field}}` placeholders, filled from the triggering event. Logic
/// beyond a single tool call belongs in a (dynamic) tool, not here.
#[derive(Clone)]
pub struct Automation {
    pub name: String,
    pub enabled: bool,
    pub trigger: Trigger,
    pub tool: String,
    pub args: serde_json::Value,
}

#[derive(Clone)]
pub enum Trigger {
    /// Recurring, standard cron expression, evaluated in local time.
    Cron {
        expression: String,
        schedule: Box<cron::Schedule>,
    },
    /// One-shot, fires once at the given instant.
    At {
        datetime: chrono::DateTime<chrono::FixedOffset>,
    },
    /// Fires on bus events. `None` fields match anything.
    Event {
        sender: Option<Sender>,
        channel: Option<String>,
    },
}

impl Trigger {
    /// Accepts the common 5-field cron form (min hour dom mon dow) as well
    /// as the 6/7-field form with seconds (and optional year).
    pub fn cron(expression: &str) -> Result<Self, String> {
        let expression = match expression.split_whitespace().count() {
            5 => format!("0 {expression}"),
            _ => expression.to_string(),
        };
        let schedule = cron::Schedule::from_str(&expression)
            .map_err(|err| format!("invalid cron expression: {err}"))?;
        Ok(Trigger::Cron {
            expression,
            schedule: Box::new(schedule),
        })
    }

    /// Accepts RFC 3339 (`2026-06-11T09:00:00+02:00`); a timestamp without
    /// an offset is interpreted as local time. A timestamp in the recent
    /// past fires immediately — the model computes the time while reasoning,
    /// but several tool round-trips may pass before this call lands, so a
    /// short "in N seconds" can legitimately arrive already overdue. Only
    /// clearly-stale timestamps are rejected.
    pub fn at(datetime: &str) -> Result<Self, String> {
        const GRACE: chrono::Duration = chrono::Duration::minutes(5);
        let datetime = parse_datetime(datetime)?;
        if datetime <= chrono::Local::now() - GRACE {
            return Err(format!("'{datetime}' is in the past"));
        }
        Ok(Trigger::At { datetime })
    }

    fn matches(&self, event: &Event) -> bool {
        match (self, event) {
            (
                Trigger::Event { sender, channel },
                Event::MessageIn {
                    sender: event_sender,
                    channel: event_channel,
                    ..
                },
            ) => {
                sender.as_ref().is_none_or(|sender| sender == event_sender)
                    && channel
                        .as_ref()
                        .is_none_or(|channel| channel == event_channel)
            }
            _ => false,
        }
    }
}

/// Holds the automations, watches the bus for event triggers, and runs a
/// timer task per cron/at automation.
pub struct Automations {
    bus: Bus,
    tools: Arc<InternalTools>,
    automations: RwLock<Vec<Arc<Automation>>>,
    store: Option<Arc<SqliteAutomations>>,
}

impl Automations {
    pub fn new(bus: Bus, tools: Arc<InternalTools>, store: Option<Arc<SqliteAutomations>>) -> Self {
        Self {
            bus,
            tools,
            automations: RwLock::new(Vec::new()),
            store,
        }
    }

    /// The default automation: every user message goes to the stock
    /// `answer_the_user` tool.
    pub fn answer_the_user() -> Automation {
        Automation {
            name: "answer-the-user".to_string(),
            enabled: true,
            trigger: Trigger::Event {
                sender: Some(Sender::User),
                channel: None,
            },
            tool: "answer_the_user".to_string(),
            args: serde_json::json!({
                "message": "{{event.text}}",
                "conversation_id": "{{event.conversation_id}}",
            }),
        }
    }

    /// Register an automation. Cron and at automations get their timer task
    /// spawned immediately.
    pub fn add(self: &Arc<Self>, automation: Automation) -> Result<(), String> {
        self.add_runtime(automation)
    }

    pub fn add_persistent(self: &Arc<Self>, automation: Automation) -> Result<(), String> {
        self.ensure_unique_name(&automation.name)?;
        if let Some(store) = &self.store {
            store.insert(&automation)?;
        }
        self.add_runtime(automation)
    }

    pub fn load_stored(self: &Arc<Self>) -> Result<usize, String> {
        let Some(store) = &self.store else {
            return Ok(0);
        };
        let automations = store.load_all()?;
        let count = automations.len();
        for automation in automations {
            self.add_runtime(automation)?;
        }
        Ok(count)
    }

    pub fn delete(&self, name: &str) -> Result<(), String> {
        if name == "answer-the-user" {
            return Err("cannot delete the built-in answer-the-user automation".to_string());
        }
        let removed = self.remove_runtime(name);
        if !removed {
            return Err(format!("unknown automation: {name}"));
        }
        if let Some(store) = &self.store {
            store.delete(name)?;
        }
        Ok(())
    }

    fn add_runtime(self: &Arc<Self>, automation: Automation) -> Result<(), String> {
        self.ensure_unique_name(&automation.name)?;
        let automation = Arc::new(automation);
        self.automations
            .write()
            .expect("automations lock")
            .push(Arc::clone(&automation));
        match &automation.trigger {
            Trigger::At { datetime } => self.spawn_at(automation.clone(), *datetime),
            Trigger::Cron { .. } => self.spawn_cron(automation.clone()),
            Trigger::Event { .. } => {}
        }
        Ok(())
    }

    fn ensure_unique_name(&self, name: &str) -> Result<(), String> {
        if self
            .automations
            .read()
            .expect("automations lock")
            .iter()
            .any(|automation| automation.name == name)
        {
            return Err(format!("an automation named '{name}' already exists"));
        }
        Ok(())
    }

    fn is_registered(&self, name: &str) -> bool {
        self.automations
            .read()
            .expect("automations lock")
            .iter()
            .any(|automation| automation.name == name)
    }

    fn remove_runtime(&self, name: &str) -> bool {
        let mut automations = self.automations.write().expect("automations lock");
        let before = automations.len();
        automations.retain(|automation| automation.name != name);
        automations.len() != before
    }

    /// Watch the bus and run event-triggered automations.
    pub fn start(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = Arc::clone(self);
        let mut events = self.bus.subscribe();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => this.handle(&event).await,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }

    async fn handle(&self, event: &Event) {
        let matching: Vec<_> = self
            .automations
            .read()
            .expect("automations lock")
            .iter()
            .filter(|automation| automation.enabled && automation.trigger.matches(event))
            .cloned()
            .collect();
        for automation in matching {
            let (ctx, event_json) = event_context(event);
            self.execute(&automation, ctx, &event_json).await;
        }
    }

    fn spawn_at(
        self: &Arc<Self>,
        automation: Arc<Automation>,
        datetime: chrono::DateTime<chrono::FixedOffset>,
    ) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let wait = (datetime.with_timezone(&chrono::Local) - chrono::Local::now())
                .to_std()
                .unwrap_or_default();
            tokio::time::sleep(wait).await;
            if !this.is_registered(&automation.name) {
                return;
            }
            let event = serde_json::json!({ "type": "at", "automation": automation.name });
            this.execute(&automation, ToolContext::default(), &event)
                .await;
            this.remove_runtime(&automation.name);
            if let Some(store) = &this.store {
                if let Err(err) = store.delete(&automation.name) {
                    eprintln!(
                        "failed to delete one-shot automation '{}': {err}",
                        automation.name
                    );
                }
            }
        });
    }

    fn spawn_cron(self: &Arc<Self>, automation: Arc<Automation>) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let Trigger::Cron { schedule, .. } = &automation.trigger else {
                return;
            };
            loop {
                let Some(next) = schedule.upcoming(chrono::Local).next() else {
                    break;
                };
                let wait = (next - chrono::Local::now()).to_std().unwrap_or_default();
                tokio::time::sleep(wait).await;
                if !this.is_registered(&automation.name) {
                    break;
                }
                let event = serde_json::json!({ "type": "cron", "automation": automation.name });
                this.execute(&automation, ToolContext::default(), &event)
                    .await;
            }
        });
    }

    async fn execute(&self, automation: &Automation, ctx: ToolContext, event: &serde_json::Value) {
        let args = render_args(&automation.args, event);
        let result = tools::run_tool(&self.tools, &automation.tool, args, &ctx).await;
        if result.get("ok").and_then(|ok| ok.as_bool()) == Some(false) {
            eprintln!(
                "automation '{}' failed: {}",
                automation.name,
                result
                    .get("error")
                    .and_then(|error| error.as_str())
                    .unwrap_or("unknown error")
            );
        }
    }
}

/// Build the tool context and the flattened event object templates see.
fn event_context(event: &Event) -> (ToolContext, serde_json::Value) {
    match event {
        Event::MessageIn {
            channel,
            sender,
            address,
            message,
        } => {
            let text = match message {
                crate::message::Message::User { text, .. } => text.clone(),
                _ => String::new(),
            };
            let ctx = ToolContext {
                destination: Some((channel.clone(), address.clone())),
                conversation_id: Some(address.conversation_id.clone()),
                ..ToolContext::default()
            };
            let event = serde_json::json!({
                "type": "message_in",
                "channel": channel,
                "sender": sender_name(sender),
                "conversation_id": address.conversation_id,
                "thread_id": address.thread_id,
                "text": text,
            });
            (ctx, event)
        }
        other => (
            ToolContext::default(),
            serde_json::json!({ "type": format!("{other:?}") }),
        ),
    }
}

/// Fill `{{event.field}}` placeholders in stored args from the triggering
/// event. A string that is exactly one placeholder takes the field's JSON
/// value; placeholders embedded in longer strings are stringified in place.
fn render_args(args: &serde_json::Value, event: &serde_json::Value) -> serde_json::Value {
    match args {
        serde_json::Value::String(text) => render_string(text, event),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(|item| render_args(item, event)).collect())
        }
        serde_json::Value::Object(object) => serde_json::Value::Object(
            object
                .iter()
                .map(|(key, value)| (key.clone(), render_args(value, event)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn render_string(text: &str, event: &serde_json::Value) -> serde_json::Value {
    let trimmed = text.trim();
    if let Some(path) = exact_placeholder(trimmed) {
        return lookup(event, path)
            .cloned()
            .unwrap_or(serde_json::Value::Null);
    }

    let mut result = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        result.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            result.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let path = after[..end].trim();
        match lookup(event, path) {
            Some(serde_json::Value::String(value)) => result.push_str(value),
            Some(value) => result.push_str(&value.to_string()),
            None => {}
        }
        rest = &after[end + 2..];
    }
    result.push_str(rest);
    serde_json::Value::String(result)
}

fn exact_placeholder(text: &str) -> Option<&str> {
    let inner = text.strip_prefix("{{")?.strip_suffix("}}")?;
    let inner = inner.trim();
    if inner.contains("{{") || inner.contains("}}") {
        return None;
    }
    Some(inner)
}

/// Look up a dot path like `event.text`; the leading `event.` is optional.
fn lookup<'a>(event: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let path = path.strip_prefix("event.").unwrap_or(path);
    let mut current = event;
    for part in path.split('.') {
        current = current.get(part)?;
    }
    Some(current)
}

pub(super) fn parse_datetime(text: &str) -> Result<chrono::DateTime<chrono::FixedOffset>, String> {
    if let Ok(datetime) = chrono::DateTime::parse_from_rfc3339(text) {
        return Ok(datetime);
    }
    // No offset: interpret as local time.
    let naive = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S")
        .map_err(|err| format!("invalid datetime '{text}' (use RFC 3339): {err}"))?;
    naive
        .and_local_timezone(chrono::Local)
        .single()
        .map(|datetime| datetime.fixed_offset())
        .ok_or_else(|| format!("ambiguous local datetime '{text}'"))
}

fn sender_name(sender: &Sender) -> &'static str {
    match sender {
        Sender::User => "user",
        Sender::Guest => "guest",
        Sender::System => "system",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bus::ChannelAddress,
        history::SqliteHistory,
        message::Message,
        tools::{InternalTools, raw_inference::RawInferenceTool, send_message::SendMessageTool},
    };

    fn test_tools(bus: &Bus) -> Arc<InternalTools> {
        let files_dir = std::env::temp_dir().join(format!(
            "warden-automation-test-files-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&files_dir).unwrap();
        let history = std::env::temp_dir().join(format!(
            "warden-automation-test-history-{}.db",
            uuid::Uuid::new_v4()
        ));
        Arc::new(InternalTools::new(
            RawInferenceTool::new("test-key".to_string()).unwrap(),
            SendMessageTool::new(bus.clone(), |_| Err("no attachments".to_string())),
            "test-key".to_string(),
            files_dir,
            Arc::new(SqliteHistory::new(history).unwrap()),
        ))
    }

    fn temp_store() -> Arc<SqliteAutomations> {
        let path = std::env::temp_dir().join(format!(
            "warden-automations-runtime-test-{}.db",
            uuid::Uuid::new_v4()
        ));
        Arc::new(SqliteAutomations::new(path).expect("store"))
    }

    fn send_message_automation(name: &str, trigger: Trigger) -> Automation {
        Automation {
            name: name.to_string(),
            enabled: true,
            trigger,
            tool: "send_message".to_string(),
            args: serde_json::json!({ "content": "hi" }),
        }
    }

    fn message_in(channel: &str, sender: Sender) -> Event {
        Event::MessageIn {
            channel: channel.to_string(),
            sender,
            address: ChannelAddress {
                channel: channel.to_string(),
                conversation_id: "c1".to_string(),
                thread_id: None,
            },
            message: Message::User {
                text: "hi".to_string(),
                attachments: Vec::new(),
            },
        }
    }

    #[test]
    fn default_automation_matches_user_messages_from_any_channel() {
        let trigger = &Automations::answer_the_user().trigger;
        assert!(trigger.matches(&message_in("tui", Sender::User)));
        assert!(trigger.matches(&message_in("telegram", Sender::User)));
        assert!(!trigger.matches(&message_in("telegram", Sender::Guest)));
        assert!(!trigger.matches(&Event::CronTick {
            schedule: "* * * * *".to_string()
        }));
    }

    #[test]
    fn channel_filter_restricts_channel() {
        let trigger = Trigger::Event {
            sender: None,
            channel: Some("telegram".to_string()),
        };
        assert!(trigger.matches(&message_in("telegram", Sender::Guest)));
        assert!(!trigger.matches(&message_in("tui", Sender::User)));
    }

    #[test]
    fn message_in_event_is_flattened_for_templates() {
        let (ctx, event) = event_context(&message_in("tui", Sender::User));
        assert_eq!(event["type"], "message_in");
        assert_eq!(event["text"], "hi");
        assert_eq!(event["conversation_id"], "c1");
        assert_eq!(ctx.conversation_id.as_deref(), Some("c1"));
        let (channel, _) = ctx.destination.expect("destination");
        assert_eq!(channel, "tui");
    }

    #[test]
    fn args_templates_fill_from_the_event() {
        let (_, event) = event_context(&message_in("tui", Sender::User));
        let args = serde_json::json!({
            "message": "{{event.text}}",
            "summary": "got '{{event.text}}' on {{event.channel}}",
            "missing": "{{event.nope}}",
            "untouched": 42,
        });

        let rendered = render_args(&args, &event);

        assert_eq!(rendered["message"], "hi");
        assert_eq!(rendered["summary"], "got 'hi' on tui");
        assert_eq!(rendered["missing"], serde_json::Value::Null);
        assert_eq!(rendered["untouched"], 42);
    }

    #[test]
    fn exact_placeholder_takes_the_json_value() {
        let event = serde_json::json!({ "thread_id": null, "count": 3 });
        assert_eq!(
            render_args(&serde_json::json!("{{event.count}}"), &event),
            serde_json::json!(3)
        );
        assert_eq!(
            render_args(&serde_json::json!("{{event.thread_id}}"), &event),
            serde_json::Value::Null
        );
    }

    #[test]
    fn cron_trigger_parses_valid_expressions() {
        assert!(Trigger::cron("0 0 9 * * * *").is_ok());
        assert!(Trigger::cron("not a cron").is_err());
    }

    #[test]
    fn cron_trigger_accepts_standard_five_field_form() {
        assert!(Trigger::cron("0 9 * * 1").is_ok());
        assert!(Trigger::cron("*/5 * * * *").is_ok());
    }

    #[test]
    fn at_trigger_accepts_future_rejects_past() {
        let future = (chrono::Local::now() + chrono::Duration::hours(1)).to_rfc3339();
        assert!(Trigger::at(&future).is_ok());

        let past = (chrono::Local::now() - chrono::Duration::hours(1)).to_rfc3339();
        assert!(Trigger::at(&past).is_err());

        assert!(Trigger::at("not a date").is_err());
    }

    #[test]
    fn at_trigger_accepts_offsetless_local_datetime() {
        let future = (chrono::Local::now() + chrono::Duration::hours(1))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        assert!(Trigger::at(&future).is_ok());
    }

    #[test]
    fn delete_removes_runtime_and_persistent_automation() {
        let bus = Bus::new(4);
        let store = temp_store();
        let automations = Arc::new(Automations::new(
            bus,
            test_tools(&Bus::new(4)),
            Some(Arc::clone(&store)),
        ));
        automations
            .add_persistent(send_message_automation(
                "daily-news",
                Trigger::Event {
                    sender: Some(Sender::User),
                    channel: None,
                },
            ))
            .unwrap();

        automations.delete("daily-news").unwrap();

        assert!(!automations.is_registered("daily-news"));
        assert!(store.load_all().unwrap().is_empty());
    }

    #[test]
    fn delete_rejects_builtin_automation() {
        let bus = Bus::new(4);
        let automations = Arc::new(Automations::new(bus.clone(), test_tools(&bus), None));
        automations.add(Automations::answer_the_user()).unwrap();

        let result = automations.delete("answer-the-user");

        assert!(result.is_err());
        assert!(automations.is_registered("answer-the-user"));
    }
}
