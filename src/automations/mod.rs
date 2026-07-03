use std::sync::{Arc, RwLock};

use crate::{
    agent::Agent,
    bus::{AutomationAction, AutomationSpec, AutomationTriggerSpec, Bus, Event, Sender},
    tools::{InternalTools, ToolContext},
};

mod effect;
mod render_args;
mod store;
mod trigger;

pub use effect::AutomationEffect;
pub use store::SqliteAutomations;
pub use trigger::Trigger;

enum AutomationEvent<'a> {
    Bus(&'a Event),
    Timer {
        kind: &'static str,
        automation_name: &'a str,
    },
}

impl AutomationEvent<'_> {
    fn context(&self) -> (ToolContext, serde_json::Value) {
        match self {
            AutomationEvent::Bus(event) => match event {
                Event::MessageIn {
                    channel,
                    sender,
                    channel_user_id,
                    conversation,
                    message_id,
                    message,
                } => {
                    let text = match message {
                        crate::message::Message::User { text, .. } => text.clone(),
                        _ => String::new(),
                    };
                    let ctx = ToolContext {
                        conversation: Some(conversation.clone()),
                        ..ToolContext::default()
                    };
                    let event = serde_json::json!({
                        "type": "message_in",
                        "channel": channel,
                        "sender": sender_name(sender),
                        "channel_user_id": channel_user_id,
                        "conversation": conversation,
                        "chat_id": &conversation.chat_id,
                        "thread_id": &conversation.thread_id,
                        "message_id": message_id,
                        "text": text,
                    });
                    (ctx, event)
                }
                Event::ToolResult {
                    context,
                    tool_call_id,
                    tool_name,
                    result,
                } => {
                    let ctx = context.clone();
                    let conversation = context.conversation.as_ref();
                    let event = serde_json::json!({
                        "type": "tool_result",
                        "channel": conversation.map(|conversation| &conversation.channel),
                        "conversation": conversation,
                        "chat_id": conversation.map(|conversation| &conversation.chat_id),
                        "thread_id": conversation.and_then(|conversation| conversation.thread_id.as_ref()),
                        "tool_call_id": tool_call_id,
                        "tool_name": tool_name,
                        "result": result,
                    });
                    (ctx, event)
                }
                other => (
                    ToolContext::default(),
                    serde_json::json!({ "type": format!("{other:?}") }),
                ),
            },
            AutomationEvent::Timer {
                kind,
                automation_name,
            } => (
                ToolContext::default(),
                serde_json::json!({ "type": kind, "automation": automation_name }),
            ),
        }
    }
}

fn sender_name(sender: &Sender) -> &'static str {
    match sender {
        Sender::User => "user",
        Sender::Guest => "guest",
        Sender::System => "system",
    }
}

/// An automation is a trigger plus an effect. Effect fields may contain
/// `{{event.field}}` placeholders, filled from the triggering event.
#[derive(Clone)]
pub struct Automation {
    pub name: String,
    pub trigger: Trigger,
    pub effect: AutomationEffect,
}

#[derive(serde::Serialize)]
pub struct AutomationInfo {
    pub name: String,
    pub trigger: serde_json::Value,
    pub effect: serde_json::Value,
}

/// Holds the automations, watches the bus for event triggers, and runs a
/// timer task per cron/at automation.
pub struct Automations {
    bus: Bus,
    tools: Arc<InternalTools>,
    agent: Arc<Agent>,
    automations: RwLock<Vec<Arc<Automation>>>,
    store: Arc<SqliteAutomations>,
}

impl Automations {
    pub fn new(
        bus: Bus,
        tools: Arc<InternalTools>,
        agent: Arc<Agent>,
        store: Arc<SqliteAutomations>,
    ) -> Self {
        Self {
            bus,
            tools,
            agent,
            automations: RwLock::new(Vec::new()),
            store,
        }
    }

    /// The default automation: every user message continues the conversation.
    pub fn handle_conversation() -> Automation {
        Automation {
            name: "handle-conversation".to_string(),
            trigger: Trigger::MessageIn {
                sender: Some(Sender::User),
                channel: None,
            },
            effect: AutomationEffect::ContinueConversation,
        }
    }

    pub fn handle_tool_result() -> Automation {
        Automation {
            name: "handle-tool-result".to_string(),
            trigger: Trigger::ToolResult,
            effect: AutomationEffect::ContinueConversation,
        }
    }

    /// Register an automation. Cron and at automations get their timer task
    /// spawned immediately.
    pub fn add(self: &Arc<Self>, automation: Automation) -> Result<(), String> {
        self.ensure_unique_name(&automation.name)?;
        self.store.insert(&automation)?;
        self.add_runtime(automation)
    }

    pub fn add_transient(self: &Arc<Self>, automation: Automation) -> Result<(), String> {
        self.add_runtime(automation)
    }

    pub fn remove_transient(&self, name: &str) {
        self.remove_runtime(name);
    }

    pub fn load_stored(self: &Arc<Self>) -> Result<usize, String> {
        let automations = self.store.load_all()?;
        let count = automations.len();
        for automation in automations {
            self.add_runtime(automation)?;
        }
        Ok(count)
    }

    pub fn delete(&self, name: &str) -> Result<(), String> {
        let removed = self.remove_runtime(name);
        if !removed {
            return Err(format!("unknown automation: {name}"));
        }
        self.store.delete(name)?;
        Ok(())
    }

    pub fn recover_stale_channel_auth(self: &Arc<Self>) -> Result<(), String> {
        let names = self
            .automations
            .read()
            .expect("automations lock")
            .iter()
            .filter(|automation| automation.name.starts_with("channel_auth:"))
            .map(|automation| automation.name.clone())
            .collect::<Vec<_>>();
        if names.is_empty() {
            return Ok(());
        }
        for name in names {
            self.remove_runtime(&name);
            self.store.delete(&name)?;
        }
        if !self.is_registered("handle-conversation") {
            let automation = Automations::handle_conversation();
            self.add(automation)?;
        }
        Ok(())
    }

    pub fn list(&self) -> Vec<AutomationInfo> {
        self.automations
            .read()
            .expect("automations lock")
            .iter()
            .map(|automation| automation_info(automation))
            .collect()
    }

    pub fn inspect(&self, name: &str) -> Result<AutomationInfo, String> {
        self.automations
            .read()
            .expect("automations lock")
            .iter()
            .find(|automation| automation.name == name)
            .map(|automation| automation_info(automation))
            .ok_or_else(|| format!("unknown automation: {name}"))
    }

    pub fn contains(&self, name: &str) -> bool {
        self.is_registered(name)
    }

    pub fn automation_from_spec(&self, spec: AutomationSpec) -> Result<Automation, String> {
        if spec.name.trim().is_empty() {
            return Err("name must not be empty".to_string());
        }
        let trigger = match spec.trigger {
            AutomationTriggerSpec::At { datetime } => Trigger::at(&datetime)?,
            AutomationTriggerSpec::Cron { expression } => Trigger::cron(&expression)?,
            AutomationTriggerSpec::MessageIn { sender, channel } => {
                Trigger::MessageIn { sender, channel }
            }
            AutomationTriggerSpec::ToolResult => Trigger::ToolResult,
        };
        match &spec.effect {
            AutomationEffect::Tool { tool, args } => {
                if tool.trim().is_empty() {
                    return Err("tool must not be empty".to_string());
                }
                self.tools.registry.validate_args_for_tool(tool, args)?;
            }
            AutomationEffect::AgentTask { prompt, tools } => {
                if prompt.trim().is_empty() {
                    return Err("prompt must not be empty".to_string());
                }
                self.tools.registry.named_tools(tools)?;
            }
            AutomationEffect::ContinueConversation => {}
        }
        Ok(Automation {
            name: spec.name,
            trigger,
            effect: spec.effect,
        })
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
            Trigger::Cron { expression } => self.spawn_cron(automation.clone(), expression.clone()),
            Trigger::MessageIn { .. } | Trigger::ToolResult => {}
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

    async fn handle(self: &Arc<Self>, event: &Event) {
        if let Event::Automation { id, action } = event {
            let result = self.handle_action(action.clone());
            self.bus.publish(Event::AutomationResult {
                id: id.clone(),
                result,
            });
            return;
        }

        let matching: Vec<_> = self
            .automations
            .read()
            .expect("automations lock")
            .iter()
            .filter(|automation| automation.trigger.matches(event))
            .cloned()
            .collect();
        for automation in matching {
            self.execute(&automation, AutomationEvent::Bus(event)).await;
        }
    }

    fn handle_action(
        self: &Arc<Self>,
        action: AutomationAction,
    ) -> Result<serde_json::Value, String> {
        match action {
            AutomationAction::Create { automation } => {
                let automation = self.automation_from_spec(automation)?;
                let name = automation.name.clone();
                self.add(automation)?;
                Ok(serde_json::json!({ "ok": true, "name": name }))
            }
            AutomationAction::Delete { name } => {
                let name = name.trim();
                if name.is_empty() {
                    return Err("name must not be empty".to_string());
                }
                self.delete(name)?;
                Ok(serde_json::json!({ "ok": true, "name": name }))
            }
            AutomationAction::Inspect { name } => {
                let name = name.trim();
                if name.is_empty() {
                    return Err("name must not be empty".to_string());
                }
                serde_json::to_value(self.inspect(name)?).map_err(|err| err.to_string())
            }
            AutomationAction::List => {
                serde_json::to_value(self.list()).map_err(|err| err.to_string())
            }
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
            this.execute(
                &automation,
                AutomationEvent::Timer {
                    kind: "at",
                    automation_name: &automation.name,
                },
            )
            .await;
            this.remove_runtime(&automation.name);
            if let Err(err) = this.store.delete(&automation.name) {
                eprintln!(
                    "failed to delete one-shot automation '{}': {err}",
                    automation.name
                );
            }
        });
    }

    fn spawn_cron(self: &Arc<Self>, automation: Arc<Automation>, expression: String) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let schedule = match expression.parse::<cron::Schedule>() {
                Ok(schedule) => schedule,
                Err(err) => {
                    eprintln!("automation '{}' has invalid cron: {err}", automation.name);
                    return;
                }
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
                this.execute(
                    &automation,
                    AutomationEvent::Timer {
                        kind: "cron",
                        automation_name: &automation.name,
                    },
                )
                .await;
            }
        });
    }

    async fn execute(&self, automation: &Automation, event: AutomationEvent<'_>) {
        let (ctx, event) = event.context();
        let result = automation
            .effect
            .execute(&self.bus, &self.tools, &self.agent, &ctx, &event)
            .await;
        if result.get("ok").and_then(serde_json::Value::as_bool) == Some(false) {
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

fn automation_info(automation: &Automation) -> AutomationInfo {
    AutomationInfo {
        name: automation.name.clone(),
        trigger: trigger_json(&automation.trigger),
        effect: serde_json::to_value(&automation.effect).expect("serialize automation effect"),
    }
}

fn trigger_json(trigger: &Trigger) -> serde_json::Value {
    match trigger {
        Trigger::Cron { expression, .. } => serde_json::json!({
            "type": "cron",
            "expression": expression,
        }),
        Trigger::At { datetime } => serde_json::json!({
            "type": "at",
            "datetime": datetime.to_rfc3339(),
        }),
        Trigger::MessageIn { sender, channel } => serde_json::json!({
            "type": "message_in",
            "sender": sender,
            "channel": channel,
        }),
        Trigger::ToolResult => serde_json::json!({
            "type": "tool_result",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::Agent,
        bus::ConversationKey,
        history::SqliteHistory,
        message::Message,
        tools::{InternalTools, messages::SendMessageTool, raw_inference::RawInferenceTool},
    };

    fn test_tools(bus: &Bus) -> Arc<InternalTools> {
        let layout = crate::config::RuntimeLayout::from_root(std::env::temp_dir().join(format!(
            "warden-automation-test-runtime-{}",
            uuid::Uuid::new_v4()
        )));
        let files_dir = layout.files_dir();
        std::fs::create_dir_all(&files_dir).unwrap();
        let history = std::env::temp_dir().join(format!(
            "warden-automation-test-history-{}.db",
            uuid::Uuid::new_v4()
        ));
        let secrets = crate::secrets::Secrets::for_tests();
        Arc::new(InternalTools::new(
            bus.clone(),
            layout,
            RawInferenceTool::new(secrets.tools.clone()).unwrap(),
            SendMessageTool::new(bus.clone(), |_| Err("no attachments".to_string())),
            crate::tools::messages::DeleteMessageTool::new(bus.clone()),
            files_dir,
            Arc::new(SqliteHistory::new(history).unwrap()),
            secrets,
        ))
    }

    fn temp_store() -> Arc<SqliteAutomations> {
        let path = std::env::temp_dir().join(format!(
            "warden-automations-runtime-test-{}.db",
            uuid::Uuid::new_v4()
        ));
        Arc::new(SqliteAutomations::new(path).expect("store"))
    }

    fn test_agent(bus: &Bus, tools: &Arc<InternalTools>) -> Arc<Agent> {
        let tool_handler = Arc::new(crate::tools::ToolHandler::new(
            bus.clone(),
            Arc::clone(&tools.registry),
            Arc::clone(&tools.history),
        ));
        Arc::new(Agent::new(
            bus.clone(),
            Arc::clone(&tools.registry),
            tool_handler,
            Arc::clone(&tools.history),
            tools.raw_inference.clone(),
        ))
    }

    fn send_message_automation(name: &str, trigger: Trigger) -> Automation {
        Automation {
            name: name.to_string(),
            trigger,
            effect: AutomationEffect::Tool {
                tool: "send_message".to_string(),
                args: serde_json::json!({ "content": "hi" }),
            },
        }
    }

    fn message_in(channel: &str, sender: Sender) -> Event {
        Event::MessageIn {
            channel: channel.to_string(),
            sender,
            channel_user_id: None,
            conversation: ConversationKey {
                channel: channel.to_string(),
                chat_id: "c1".to_string(),
                thread_id: None,
            },
            message_id: None,
            message: Message::User {
                text: "hi".to_string(),
                attachments: Vec::new(),
            },
        }
    }

    #[test]
    fn message_in_event_is_flattened_for_execution() {
        let (ctx, event) = AutomationEvent::Bus(&message_in("tui", Sender::User)).context();
        assert_eq!(event["type"], "message_in");
        assert_eq!(event["text"], "hi");
        assert_eq!(event["chat_id"], "c1");
        assert_eq!(ctx.conversation.as_ref().unwrap().chat_id, "c1");
        assert_eq!(ctx.conversation.as_ref().unwrap().channel, "tui");
    }

    #[test]
    fn delete_removes_runtime_and_stored_automation() {
        let bus = Bus::new(4);
        let store = temp_store();
        let tools = test_tools(&bus);
        let automations = Arc::new(Automations::new(
            bus.clone(),
            Arc::clone(&tools),
            test_agent(&bus, &tools),
            Arc::clone(&store),
        ));
        automations
            .add(send_message_automation(
                "daily-news",
                Trigger::MessageIn {
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
    fn delete_allows_builtin_automation() {
        let bus = Bus::new(4);
        let tools = test_tools(&bus);
        let automations = Arc::new(Automations::new(
            bus.clone(),
            Arc::clone(&tools),
            test_agent(&bus, &tools),
            temp_store(),
        ));
        automations.add(Automations::handle_conversation()).unwrap();

        let result = automations.delete("handle-conversation");

        assert!(result.is_ok());
        assert!(!automations.is_registered("handle-conversation"));
    }
}
