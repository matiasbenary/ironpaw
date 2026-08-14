use std::sync::{Arc, RwLock};

use crate::{
    bus::{AutomationAction, Bus, ConversationKey, Event, MessageIn, SenderKind},
    tools::ToolRegistry,
};

mod effect;
mod invocation;
mod render_args;
mod store;
mod trigger;

pub use effect::{AgentTaskEffect, AutomationEffect, ToolEffect, UserMessageEffect};
pub use store::FileAutomations;
pub use trigger::Schedule;

#[derive(Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Automation {
    /// Run an effect whenever an inbound user message matches the conversation.
    OnUserMessageIn {
        /// Short unique kebab-case name.
        name: String,
        /// Exact conversation to match. Omit to match every conversation.
        #[serde(default)]
        conversation: Option<ConversationKey>,
        effect: UserMessageEffect,
    },
    /// Run a bounded effect whenever an inbound guest message matches the conversation.
    OnGuestMessageIn {
        /// Short unique kebab-case name.
        name: String,
        /// Exact conversation to match. Omit to match every conversation.
        #[serde(default)]
        conversation: Option<ConversationKey>,
        effect: AutomationEffect,
    },
    /// Run an effect once at an absolute time or repeatedly on a cron schedule.
    Scheduled {
        /// Short unique kebab-case name.
        name: String,
        schedule: Schedule,
        effect: AutomationEffect,
    },
}

impl Automation {
    pub fn name(&self) -> &str {
        match self {
            Self::OnUserMessageIn { name, .. }
            | Self::OnGuestMessageIn { name, .. }
            | Self::Scheduled { name, .. } => name,
        }
    }
}

/// Holds the automations, watches the bus for event triggers, and runs a
/// timer task per cron/at automation.
pub struct Automations {
    bus: Bus,
    tools: Arc<ToolRegistry>,
    automations: RwLock<Vec<Arc<Automation>>>,
    store: Arc<FileAutomations>,
}

impl Automations {
    pub fn new(bus: Bus, tools: Arc<ToolRegistry>, store: Arc<FileAutomations>) -> Self {
        Self {
            bus,
            tools,
            automations: RwLock::new(Vec::new()),
            store,
        }
    }

    /// Register an automation. Cron and at automations get their timer task
    /// spawned immediately.
    pub fn add_persistent(self: &Arc<Self>, automation: Automation) -> Result<(), String> {
        self.validate_automation(&automation)?;
        self.ensure_unique_name(automation.name())?;
        self.store.write(&automation)?;
        self.add_runtime(automation)
    }

    pub fn load_stored(self: &Arc<Self>) -> Result<usize, String> {
        let automations = self.store.load_all()?;
        let count = automations.len();
        for automation in automations {
            self.validate_automation(&automation)?;
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

    pub fn list(&self) -> Vec<Automation> {
        self.automations
            .read()
            .expect("automations lock")
            .iter()
            .map(|automation| automation.as_ref().clone())
            .collect()
    }

    pub fn inspect(&self, name: &str) -> Result<Automation, String> {
        self.automations
            .read()
            .expect("automations lock")
            .iter()
            .find(|automation| automation.name() == name)
            .map(|automation| automation.as_ref().clone())
            .ok_or_else(|| format!("unknown automation: {name}"))
    }

    pub fn contains(&self, name: &str) -> bool {
        self.automations
            .read()
            .expect("automations lock")
            .iter()
            .any(|automation| automation.name() == name)
    }

    fn validate_automation(&self, automation: &Automation) -> Result<(), String> {
        if automation.name().trim().is_empty() {
            return Err("name must not be empty".to_string());
        }
        match automation {
            Automation::OnUserMessageIn { effect, .. } => match effect {
                UserMessageEffect::Tool(effect) => self.validate_tool_effect(effect),
                UserMessageEffect::AgentTask(effect) => self.validate_agent_task_effect(effect),
                UserMessageEffect::ContinueConversation => Ok(()),
            },
            Automation::OnGuestMessageIn { effect, .. } => match effect {
                AutomationEffect::Tool(effect) => self.validate_tool_effect(effect),
                AutomationEffect::AgentTask(effect) => self.validate_agent_task_effect(effect),
            },
            Automation::Scheduled {
                schedule, effect, ..
            } => {
                schedule.validate()?;
                match effect {
                    AutomationEffect::Tool(effect) => self.validate_tool_effect(effect),
                    AutomationEffect::AgentTask(effect) => self.validate_agent_task_effect(effect),
                }
            }
        }
    }

    fn validate_tool_effect(&self, effect: &ToolEffect) -> Result<(), String> {
        if effect.tool.trim().is_empty() {
            return Err("tool must not be empty".to_string());
        }
        self.tools
            .validate_args_for_tool(&effect.tool, &effect.args)
    }

    fn validate_agent_task_effect(&self, effect: &AgentTaskEffect) -> Result<(), String> {
        if effect.prompt.trim().is_empty() {
            return Err("prompt must not be empty".to_string());
        }
        self.tools.named_tools(&effect.tools).map(|_| ())
    }

    pub fn add_runtime(self: &Arc<Self>, automation: Automation) -> Result<(), String> {
        self.ensure_unique_name(automation.name())?;
        let automation = Arc::new(automation);
        self.automations
            .write()
            .expect("automations lock")
            .push(Arc::clone(&automation));
        match &*automation {
            Automation::Scheduled {
                schedule: Schedule::At { datetime },
                ..
            } => self.spawn_at(automation.clone(), *datetime),
            Automation::Scheduled {
                schedule: Schedule::Cron { expression },
                ..
            } => self.spawn_cron(automation.clone(), expression.clone()),
            Automation::OnUserMessageIn { .. } | Automation::OnGuestMessageIn { .. } => {}
        }
        Ok(())
    }

    fn ensure_unique_name(&self, name: &str) -> Result<(), String> {
        if self
            .automations
            .read()
            .expect("automations lock")
            .iter()
            .any(|automation| automation.name() == name)
        {
            return Err(format!("an automation named '{name}' already exists"));
        }
        Ok(())
    }

    pub fn remove_runtime(&self, name: &str) -> bool {
        let mut automations = self.automations.write().expect("automations lock");
        let before = automations.len();
        automations.retain(|automation| automation.name() != name);
        let removed = automations.len() != before;
        drop(automations);
        removed
    }

    /// Watch the bus and run event-triggered automations.
    pub fn start(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = Arc::clone(self);
        let mut events = self.bus.subscribe();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => this.handle(&event),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }

    fn handle(self: &Arc<Self>, event: &Event) {
        match event {
            Event::Automation { id, action } => {
                let result = self.handle_action(action.clone());
                self.bus.publish(Event::AutomationResult {
                    id: id.clone(),
                    result,
                });
            }
            Event::MessageIn(message) => self.handle_message(message),
            _ => {}
        }
    }

    fn handle_message(&self, message: &MessageIn) {
        let matching = self
            .automations
            .read()
            .expect("automations lock")
            .iter()
            .filter_map(|automation| match &**automation {
                Automation::OnUserMessageIn { conversation, .. }
                    if message.sender.kind() == SenderKind::User
                        && conversation
                            .as_ref()
                            .is_none_or(|expected| expected == &message.conversation) =>
                {
                    Some(Arc::clone(automation))
                }
                Automation::OnGuestMessageIn { conversation, .. }
                    if message.sender.kind() == SenderKind::Guest
                        && conversation
                            .as_ref()
                            .is_none_or(|expected| expected == &message.conversation) =>
                {
                    Some(Arc::clone(automation))
                }
                Automation::OnUserMessageIn { .. } | Automation::OnGuestMessageIn { .. } => None,
                Automation::Scheduled { .. } => None,
            })
            .collect::<Vec<_>>();
        if matching.is_empty() {
            if message.sender.kind() == SenderKind::User {
                let context = crate::tools::ExecutionContext::new(
                    crate::tools::InvocationScope::Conversation(message.conversation.clone()),
                );
                self.bus.publish(Event::AgentRequest {
                    execution_id: uuid::Uuid::new_v4().to_string(),
                    request: crate::bus::AgentRequest::ContinueConversation {
                        conversation: message.conversation.clone(),
                        context,
                    },
                });
            }
            return;
        }
        let context = crate::tools::ExecutionContext::new(
            crate::tools::InvocationScope::Conversation(message.conversation.clone()),
        );
        let event = serde_json::to_value(message).expect("serialize inbound message");
        for automation in matching {
            self.invoke(automation.name(), context.clone(), event.clone());
        }
    }

    fn handle_action(
        self: &Arc<Self>,
        action: AutomationAction,
    ) -> Result<serde_json::Value, String> {
        match action {
            AutomationAction::Create { automation } => {
                let name = automation.name().to_string();
                self.add_persistent(automation)?;
                Ok(serde_json::json!({ "name": name }))
            }
            AutomationAction::Edit { automation } => {
                self.validate_automation(&automation)?;
                let name = automation.name().to_string();
                if !self.remove_runtime(&name) {
                    return Err(format!("unknown automation: {name}"));
                }
                self.store.write(&automation)?;
                self.add_runtime(automation)?;
                Ok(serde_json::json!({ "name": name }))
            }
            AutomationAction::Delete { name } => {
                self.delete(&name)?;
                Ok(serde_json::json!({ "ok": true, "name": name }))
            }
            AutomationAction::Inspect { name } => {
                serde_json::to_value(self.inspect(&name)?).map_err(|err| err.to_string())
            }
            AutomationAction::List => {
                serde_json::to_value(self.list()).map_err(|err| err.to_string())
            }
            AutomationAction::Trigger { name } => {
                let automation = self
                    .automations
                    .read()
                    .expect("automations lock")
                    .iter()
                    .find(|automation| automation.name() == name)
                    .cloned()
                    .ok_or_else(|| format!("unknown automation: {name}"))?;
                if !matches!(&*automation, Automation::Scheduled { .. }) {
                    return Err("message automations cannot be triggered manually".to_string());
                }
                let (context, trigger) = invocation::from_timer("manual", automation.name());
                let execution_id = self.invoke(automation.name(), context, trigger);
                Ok(serde_json::json!({
                    "ok": true,
                    "name": name,
                    "execution_id": execution_id,
                }))
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
            if !this.contains(automation.name()) {
                return;
            }
            let (context, trigger) = invocation::from_timer("at", automation.name());
            this.invoke(automation.name(), context, trigger);
        });
    }

    fn spawn_cron(self: &Arc<Self>, automation: Arc<Automation>, expression: String) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let schedule = match Schedule::cron(&expression) {
                Ok(Schedule::Cron { expression }) => expression.parse::<cron::Schedule>(),
                Ok(Schedule::At { .. }) => unreachable!(),
                Err(err) => {
                    eprintln!("automation '{}' has invalid cron: {err}", automation.name());
                    return;
                }
            };
            let schedule = schedule.expect("validated cron schedule");
            while let Some(next) = schedule.upcoming(chrono::Local).next() {
                let wait = (next - chrono::Local::now()).to_std().unwrap_or_default();
                tokio::time::sleep(wait).await;
                if !this.contains(automation.name()) {
                    break;
                }
                let (context, trigger) = invocation::from_timer("cron", automation.name());
                this.invoke(automation.name(), context, trigger);
            }
        });
    }

    fn invoke(
        &self,
        automation_id: &str,
        context: crate::tools::ExecutionContext,
        trigger: serde_json::Value,
    ) -> String {
        let execution_id = uuid::Uuid::new_v4().to_string();
        let automation = self
            .automations
            .read()
            .expect("automations lock")
            .iter()
            .find(|automation| automation.name() == automation_id)
            .cloned();
        let Some(automation) = automation else {
            eprintln!("automation '{automation_id}' disappeared before execution");
            return execution_id;
        };
        match &*automation {
            Automation::OnUserMessageIn { effect, .. } => {
                effect.dispatch(&self.bus, &execution_id, automation_id, &context, &trigger)
            }
            Automation::OnGuestMessageIn { effect, .. } => {
                effect.dispatch(&self.bus, &execution_id, automation_id, &context, &trigger)
            }
            Automation::Scheduled { effect, .. } => {
                effect.dispatch(&self.bus, &execution_id, automation_id, &context, &trigger)
            }
        }
        if matches!(
            &*automation,
            Automation::Scheduled {
                schedule: Schedule::At { .. },
                ..
            }
        ) && let Err(err) = self.delete(automation_id)
        {
            eprintln!("failed to delete one-shot automation '{automation_id}': {err}");
        }
        execution_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bus::ConversationKey,
        history::SqliteHistory,
        inference::Inference,
        tools::{ExecutionContext, InternalTools, InvocationScope, messages::SendMessageTool},
    };

    fn test_tools(bus: &Bus) -> Arc<InternalTools> {
        let layout = crate::config::RuntimeLayout::from_root(std::env::temp_dir().join(format!(
            "ironpaw-automation-test-runtime-{}",
            uuid::Uuid::new_v4()
        )));
        let files_dir = layout.files_dir();
        std::fs::create_dir_all(&files_dir).unwrap();
        let history = std::env::temp_dir().join(format!(
            "ironpaw-automation-test-history-{}.db",
            uuid::Uuid::new_v4()
        ));
        let secrets = crate::secrets::Secrets::for_tests();
        Arc::new(InternalTools::new(
            bus.clone(),
            layout,
            Inference::new(secrets.tools.clone()).unwrap(),
            SendMessageTool::new(bus.clone(), |_, _| Err("no attachments".to_string())),
            crate::tools::messages::DeleteMessageTool::new(bus.clone()),
            files_dir,
            Arc::new(SqliteHistory::new(history).unwrap()),
            secrets,
        ))
    }

    fn temp_store() -> Arc<FileAutomations> {
        let path = std::env::temp_dir().join(format!(
            "ironpaw-automations-runtime-test-{}",
            uuid::Uuid::new_v4()
        ));
        Arc::new(FileAutomations::new(path).expect("store"))
    }

    fn scheduled_send_message_automation(name: &str, schedule: Schedule) -> Automation {
        Automation::Scheduled {
            name: name.to_string(),
            schedule,
            effect: AutomationEffect::Tool(ToolEffect {
                tool: "send_message".to_string(),
                args: serde_json::json!({
                    "channel": "tui",
                    "chat_id": "c1",
                    "content": "hi",
                }),
            }),
        }
    }

    fn message_send_message_automation(
        name: &str,
        conversation: Option<ConversationKey>,
    ) -> Automation {
        Automation::OnUserMessageIn {
            name: name.to_string(),
            conversation,
            effect: UserMessageEffect::Tool(ToolEffect {
                tool: "send_message".to_string(),
                args: serde_json::json!({
                    "channel": "tui",
                    "chat_id": "c1",
                    "content": "hi",
                }),
            }),
        }
    }

    fn agent_task_automation(name: &str) -> Automation {
        Automation::Scheduled {
            name: name.to_string(),
            schedule: Schedule::cron("0 0 * * * * *").unwrap(),
            effect: AutomationEffect::AgentTask(AgentTaskEffect {
                prompt: "do the work".to_string(),
                tools: Vec::new(),
            }),
        }
    }

    fn user_message(conversation: ConversationKey) -> Event {
        Event::MessageIn(MessageIn {
            sender: crate::bus::Sender::User("owner".to_string()),
            conversation,
            message_id: None,
            text: "hello".to_string(),
            attachments: Vec::new(),
        })
    }

    #[tokio::test]
    async fn exact_conversation_trigger_overrides_conversation_fallback() {
        let bus = Bus::new(16);
        let tools = test_tools(&bus);
        let automations = Arc::new(Automations::new(
            bus.clone(),
            Arc::clone(&tools.registry),
            temp_store(),
        ));
        let scoped_conversation = ConversationKey {
            channel: "telegram".to_string(),
            chat_id: "setup".to_string(),
            thread_id: None,
        };
        automations
            .add_runtime(message_send_message_automation(
                "onboarding",
                Some(scoped_conversation.clone()),
            ))
            .unwrap();
        let mut events = bus.subscribe();

        automations.handle(&user_message(scoped_conversation));
        assert!(matches!(
            events.recv().await.unwrap(),
            Event::ToolRequest { .. }
        ));
        assert!(matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));

        automations.handle(&user_message(ConversationKey {
            channel: "telegram".to_string(),
            chat_id: "other".to_string(),
            thread_id: None,
        }));
        assert!(matches!(
            events.recv().await.unwrap(),
            Event::AgentRequest { .. }
        ));
    }

    #[tokio::test]
    async fn create_and_edit_automation_tools_persist_the_definition() {
        let bus = Bus::new(16);
        let tools = test_tools(&bus);
        let store = temp_store();
        let automations = Arc::new(Automations::new(
            bus.clone(),
            Arc::clone(&tools.registry),
            Arc::clone(&store),
        ));
        let task = automations.start();
        let context = ExecutionContext::new(InvocationScope::Conversation(ConversationKey {
            channel: "tui".to_string(),
            chat_id: "c1".to_string(),
            thread_id: None,
        }));

        crate::tools::run_tool(
            &tools,
            "create_automation",
            serde_json::json!({
                "type": "scheduled",
                "name": "later",
                "schedule": {
                    "type": "at",
                    "datetime": (chrono::Local::now() + chrono::Duration::hours(1)).to_rfc3339()
                },
                "effect": {
                    "type": "tool",
                    "tool": "send_message",
                    "args": { "channel": "tui", "content": "first" }
                },
            }),
            &context,
        )
        .await
        .unwrap();
        crate::tools::run_tool(
            &tools,
            "edit_automation",
            serde_json::json!({
                "type": "scheduled",
                "name": "later",
                "schedule": {
                    "type": "at",
                    "datetime": (chrono::Local::now() + chrono::Duration::hours(2)).to_rfc3339()
                },
                "effect": {
                    "type": "tool",
                    "tool": "send_message",
                    "args": { "channel": "tui", "content": "second" }
                },
            }),
            &context,
        )
        .await
        .unwrap();

        let stored = store.load_all().unwrap();
        assert_eq!(stored.len(), 1);
        let Automation::Scheduled {
            effect: AutomationEffect::Tool(effect),
            ..
        } = &stored[0]
        else {
            panic!("expected tool automation");
        };
        assert_eq!(effect.args["content"], "second");
        task.abort();
    }

    #[tokio::test]
    async fn create_automation_tool_accepts_on_guest_message_in() {
        let bus = Bus::new(16);
        let tools = test_tools(&bus);
        let automations = Arc::new(Automations::new(
            bus.clone(),
            Arc::clone(&tools.registry),
            temp_store(),
        ));
        let task = automations.start();
        let mut events = bus.subscribe();
        let conversation = ConversationKey {
            channel: "telegram".to_string(),
            chat_id: "c1".to_string(),
            thread_id: None,
        };
        let context = ExecutionContext::new(InvocationScope::Conversation(conversation.clone()));

        crate::tools::run_tool(
            &tools,
            "create_automation",
            serde_json::json!({
                "type": "on_guest_message_in",
                "name": "greet-guests",
                "effect": {
                    "type": "tool",
                    "tool": "send_message",
                    "args": {
                        "channel": "tui",
                        "chat_id": "{{event.sender.channel_user_id}}",
                        "content": "{{event.text}}"
                    }
                }
            }),
            &context,
        )
        .await
        .unwrap();

        automations.handle(&Event::MessageIn(MessageIn {
            sender: crate::bus::Sender::Guest("guest-1".to_string()),
            conversation,
            message_id: None,
            text: "hello".to_string(),
            attachments: Vec::new(),
        }));

        let arguments = loop {
            if let Event::ToolRequest { tool_calls, .. } = events.recv().await.unwrap() {
                break serde_json::from_str::<serde_json::Value>(&tool_calls[0].arguments).unwrap();
            }
        };
        assert_eq!(arguments["chat_id"], "guest-1");
        assert_eq!(arguments["content"], "hello");
        task.abort();
    }

    #[tokio::test]
    async fn agent_task_invocations_are_forwarded_to_agent() {
        let bus = Bus::new(16);
        let tools = test_tools(&bus);
        let automations = Arc::new(Automations::new(
            bus.clone(),
            Arc::clone(&tools.registry),
            temp_store(),
        ));
        automations
            .add_persistent(agent_task_automation("serial"))
            .unwrap();
        let task = automations.start();
        let mut events = bus.subscribe();
        let context = ExecutionContext::new(InvocationScope::ScheduledAutomation {
            automation_id: "serial".to_string(),
        });
        let first = automations.invoke("serial", context.clone(), serde_json::json!({}));
        let second = automations.invoke("serial", context, serde_json::json!({}));

        let requests = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            let mut requests = Vec::new();
            loop {
                if let Event::AgentRequest { execution_id, .. } =
                    events.recv().await.expect("event")
                {
                    requests.push(execution_id);
                    if requests.len() == 2 {
                        break requests;
                    }
                }
            }
        })
        .await
        .expect("agent requests");
        task.abort();

        assert_eq!(requests, vec![first, second]);
    }

    #[tokio::test]
    async fn one_shot_is_deleted_after_dispatch() {
        let bus = Bus::new(16);
        let tools = test_tools(&bus);
        let store = temp_store();
        let automations = Arc::new(Automations::new(
            bus.clone(),
            Arc::clone(&tools.registry),
            Arc::clone(&store),
        ));
        let datetime = (chrono::Local::now() + chrono::Duration::milliseconds(50)).fixed_offset();
        automations
            .add_persistent(scheduled_send_message_automation(
                "once",
                Schedule::At { datetime },
            ))
            .unwrap();
        let task = automations.start();
        let mut events = bus.subscribe();

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if matches!(
                    events.recv().await.expect("event"),
                    Event::ToolRequest { .. }
                ) {
                    break;
                }
            }
        })
        .await
        .expect("tool request");

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while automations.contains("once") {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("one-shot deletion");
        assert!(store.load_all().unwrap().is_empty());
        task.abort();
    }

    #[test]
    fn delete_removes_runtime_and_stored_automation() {
        let bus = Bus::new(4);
        let store = temp_store();
        let tools = test_tools(&bus);
        let automations = Arc::new(Automations::new(
            bus.clone(),
            Arc::clone(&tools.registry),
            Arc::clone(&store),
        ));
        automations
            .add_persistent(message_send_message_automation("daily-news", None))
            .unwrap();

        automations.delete("daily-news").unwrap();

        assert!(!automations.contains("daily-news"));
        assert!(store.load_all().unwrap().is_empty());
    }
}
