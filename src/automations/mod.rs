use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex, RwLock},
};

use crate::{
    bus::{AutomationAction, AutomationSpec, AutomationTriggerSpec, Bus, Event, SenderKind},
    tools::ToolRegistry,
};

mod effect;
mod invocation;
mod render_args;
mod store;
mod trigger;

pub use effect::AutomationEffect;
pub use store::FileAutomations;
pub use trigger::Trigger;

/// An automation is a trigger plus an effect. Effect fields may contain
/// `{{event.field}}` placeholders, filled from the triggering event.
#[derive(Clone, serde::Deserialize, serde::Serialize)]
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
    tools: Arc<ToolRegistry>,
    automations: RwLock<Vec<Arc<Automation>>>,
    store: Arc<FileAutomations>,
    agent_task_queues: Mutex<HashMap<String, AgentTaskQueue>>,
}

#[derive(Default)]
struct AgentTaskQueue {
    running: Option<String>,
    pending: VecDeque<PendingInvocation>,
}

struct PendingInvocation {
    execution_id: String,
    context: crate::tools::ExecutionContext,
    trigger: serde_json::Value,
}

impl Automations {
    pub fn new(bus: Bus, tools: Arc<ToolRegistry>, store: Arc<FileAutomations>) -> Self {
        Self {
            bus,
            tools,
            automations: RwLock::new(Vec::new()),
            store,
            agent_task_queues: Mutex::new(HashMap::new()),
        }
    }

    /// The default automation: every user message continues the conversation.
    pub fn handle_conversation() -> Automation {
        Automation {
            name: "handle-conversation".to_string(),
            trigger: Trigger::MessageIn {
                sender: SenderKind::User,
                conversation: None,
            },
            effect: AutomationEffect::ContinueConversation,
        }
    }

    /// Register an automation. Cron and at automations get their timer task
    /// spawned immediately.
    pub fn add(self: &Arc<Self>, automation: Automation) -> Result<(), String> {
        self.ensure_unique_name(&automation.name)?;
        self.store.write(&automation)?;
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

    pub fn recover_stale_setup(self: &Arc<Self>) -> Result<(), String> {
        let names = self
            .automations
            .read()
            .expect("automations lock")
            .iter()
            .filter(|automation| {
                automation.name.starts_with("channel_auth:")
                    || automation.name.starts_with("tool_auth:")
            })
            .map(|automation| automation.name.clone())
            .collect::<Vec<_>>();
        if names.is_empty() {
            return Ok(());
        }
        for name in names {
            self.remove_runtime(&name);
            self.store.delete(&name)?;
        }
        if !self.contains("handle-conversation") {
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
        self.automations
            .read()
            .expect("automations lock")
            .iter()
            .any(|automation| automation.name == name)
    }

    pub fn automation_from_spec(&self, spec: AutomationSpec) -> Result<Automation, String> {
        if spec.name.trim().is_empty() {
            return Err("name must not be empty".to_string());
        }
        let trigger = match spec.trigger {
            AutomationTriggerSpec::At { datetime } => Trigger::at(&datetime)?,
            AutomationTriggerSpec::Cron { expression } => Trigger::cron(&expression)?,
            AutomationTriggerSpec::MessageIn {
                sender,
                conversation,
            } => Trigger::MessageIn {
                sender,
                conversation,
            },
            AutomationTriggerSpec::ToolResult => Trigger::ToolResult,
            AutomationTriggerSpec::ApiKeyError => Trigger::ApiKeyError,
        };
        let automation = Automation {
            name: spec.name,
            trigger,
            effect: spec.effect,
        };
        self.validate_automation(&automation)?;
        Ok(automation)
    }

    fn validate_automation(&self, automation: &Automation) -> Result<(), String> {
        if automation.name.trim().is_empty() {
            return Err("name must not be empty".to_string());
        }
        match &automation.effect {
            AutomationEffect::Tool { tool, args } => {
                if tool.trim().is_empty() {
                    return Err("tool must not be empty".to_string());
                }
                self.tools.validate_args_for_tool(tool, args)?;
            }
            AutomationEffect::AgentTask { prompt, tools } => {
                if prompt.trim().is_empty() {
                    return Err("prompt must not be empty".to_string());
                }
                self.tools.named_tools(tools)?;
            }
            AutomationEffect::ContinueConversation => {}
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
            Trigger::Cron { expression } => self.spawn_cron(automation.clone(), expression.clone()),
            Trigger::MessageIn { .. } | Trigger::ToolResult | Trigger::ApiKeyError => {}
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

    fn remove_runtime(&self, name: &str) -> bool {
        let mut automations = self.automations.write().expect("automations lock");
        let before = automations.len();
        automations.retain(|automation| automation.name != name);
        let removed = automations.len() != before;
        drop(automations);
        if removed {
            self.agent_task_queues
                .lock()
                .expect("agent task queues lock")
                .remove(name);
        }
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
        if let Event::AgentResult { execution_id, .. } = event
            && let Some((automation_id, invocation)) = self.complete_agent_task(execution_id)
        {
            self.start_agent_task(&automation_id, invocation);
        }
        if let Event::Automation { id, action } = event {
            let result = self.handle_action(action.clone());
            self.bus.publish(Event::AutomationResult {
                id: id.clone(),
                result,
            });
            return;
        }
        if let Event::AutomationInvocation {
            execution_id,
            automation_id,
            context,
            trigger,
        } = event
        {
            self.execute_invocation(execution_id, automation_id, context, trigger);
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
        let Some((context, trigger)) = invocation::from_bus(event) else {
            return;
        };
        for automation in matching {
            self.dispatch(&automation.name, context.clone(), trigger.clone());
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
                Ok(serde_json::json!({ "name": name }))
            }
            AutomationAction::Edit { automation } => {
                let automation = self.automation_from_spec(automation)?;
                let name = automation.name.clone();
                if !self.remove_runtime(&name) {
                    return Err(format!("unknown automation: {name}"));
                }
                self.store.write(&automation)?;
                self.add_runtime(automation)?;
                Ok(serde_json::json!({ "name": name }))
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
            AutomationAction::Trigger { name } => {
                let name = name.trim();
                if name.is_empty() {
                    return Err("name must not be empty".to_string());
                }
                let automation = self
                    .automations
                    .read()
                    .expect("automations lock")
                    .iter()
                    .find(|automation| automation.name == name)
                    .cloned()
                    .ok_or_else(|| format!("unknown automation: {name}"))?;
                let (context, trigger) = invocation::from_timer("manual", &automation.name);
                let execution_id = self.dispatch(&automation.name, context, trigger);
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
            if !this.contains(&automation.name) {
                return;
            }
            let (context, trigger) = invocation::from_timer("at", &automation.name);
            this.dispatch(&automation.name, context, trigger);
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
            while let Some(next) = schedule.upcoming(chrono::Local).next() {
                let wait = (next - chrono::Local::now()).to_std().unwrap_or_default();
                tokio::time::sleep(wait).await;
                if !this.contains(&automation.name) {
                    break;
                }
                let (context, trigger) = invocation::from_timer("cron", &automation.name);
                this.dispatch(&automation.name, context, trigger);
            }
        });
    }

    fn dispatch(
        &self,
        automation_id: &str,
        context: crate::tools::ExecutionContext,
        trigger: serde_json::Value,
    ) -> String {
        let execution_id = uuid::Uuid::new_v4().to_string();
        self.bus.publish(Event::AutomationInvocation {
            execution_id: execution_id.clone(),
            automation_id: automation_id.to_string(),
            context,
            trigger,
        });
        execution_id
    }

    fn execute_invocation(
        &self,
        execution_id: &str,
        automation_id: &str,
        context: &crate::tools::ExecutionContext,
        trigger: &serde_json::Value,
    ) {
        let automation = self
            .automations
            .read()
            .expect("automations lock")
            .iter()
            .find(|automation| automation.name == automation_id)
            .cloned();
        let Some(automation) = automation else {
            eprintln!("automation '{automation_id}' disappeared before execution");
            return;
        };
        if matches!(automation.effect, AutomationEffect::AgentTask { .. }) {
            let invocation = PendingInvocation {
                execution_id: execution_id.to_string(),
                context: context.clone(),
                trigger: trigger.clone(),
            };
            let mut queues = self
                .agent_task_queues
                .lock()
                .expect("agent task queues lock");
            let queue = queues.entry(automation_id.to_string()).or_default();
            if queue.running.is_some() {
                queue.pending.push_back(invocation);
                return;
            }
            queue.running = Some(invocation.execution_id.clone());
        }
        automation
            .effect
            .dispatch(&self.bus, execution_id, automation_id, context, trigger);
        if matches!(automation.trigger, Trigger::At { .. })
            && let Err(err) = self.delete(automation_id)
        {
            eprintln!("failed to delete one-shot automation '{automation_id}': {err}");
        }
    }

    fn complete_agent_task(&self, execution_id: &str) -> Option<(String, PendingInvocation)> {
        let mut queues = self
            .agent_task_queues
            .lock()
            .expect("agent task queues lock");
        let automation_id = queues.iter().find_map(|(automation_id, queue)| {
            (queue.running.as_deref() == Some(execution_id)).then(|| automation_id.clone())
        })?;
        let queue = queues.get_mut(&automation_id).expect("queued automation");
        let next = queue.pending.pop_front();
        queue.running = next
            .as_ref()
            .map(|invocation| invocation.execution_id.clone());
        if queue.running.is_none() {
            queues.remove(&automation_id);
        }
        next.map(|invocation| (automation_id, invocation))
    }

    fn start_agent_task(&self, automation_id: &str, invocation: PendingInvocation) {
        let automation = self
            .automations
            .read()
            .expect("automations lock")
            .iter()
            .find(|automation| automation.name == automation_id)
            .cloned();
        let Some(automation) = automation else {
            return;
        };
        automation.effect.dispatch(
            &self.bus,
            &invocation.execution_id,
            automation_id,
            &invocation.context,
            &invocation.trigger,
        );
    }
}

fn automation_info(automation: &Automation) -> AutomationInfo {
    AutomationInfo {
        name: automation.name.clone(),
        trigger: serde_json::to_value(&automation.trigger).expect("serialize automation trigger"),
        effect: serde_json::to_value(&automation.effect).expect("serialize automation effect"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bus::{ConversationKey, InvocationScope, Sender},
        history::SqliteHistory,
        inference::Inference,
        message::Message,
        tools::{ExecutionContext, InternalTools, messages::SendMessageTool},
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
            Inference::new(bus.clone(), secrets.tools.clone()).unwrap(),
            SendMessageTool::new(bus.clone(), |_| Err("no attachments".to_string())),
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

    fn send_message_automation(name: &str, trigger: Trigger) -> Automation {
        Automation {
            name: name.to_string(),
            trigger,
            effect: AutomationEffect::Tool {
                tool: "send_message".to_string(),
                args: serde_json::json!({
                    "channel": "tui",
                    "chat_id": "c1",
                    "content": "hi",
                }),
            },
        }
    }

    fn agent_task_automation(name: &str) -> Automation {
        Automation {
            name: name.to_string(),
            trigger: Trigger::MessageIn {
                sender: SenderKind::User,
                conversation: None,
            },
            effect: AutomationEffect::AgentTask {
                prompt: "do the work".to_string(),
                tools: Vec::new(),
            },
        }
    }

    fn message_in(channel: &str, sender: Sender) -> Event {
        Event::MessageIn {
            sender,
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
        let (ctx, event) =
            invocation::from_bus(&message_in("tui", Sender::User("local".to_string())))
                .expect("message invocation");
        assert_eq!(event["type"], "message_in");
        assert_eq!(event["text"], "hi");
        assert_eq!(event["chat_id"], "c1");
        let InvocationScope::Conversation(conversation) = ctx.invoked_by else {
            panic!("expected conversation invocation");
        };
        assert_eq!(conversation.chat_id, "c1");
        assert_eq!(conversation.channel, "tui");
    }

    #[tokio::test]
    async fn message_trigger_dispatches_tool_request_without_waiting() {
        let bus = Bus::new(16);
        let tools = test_tools(&bus);
        let automations = Arc::new(Automations::new(
            bus.clone(),
            Arc::clone(&tools.registry),
            temp_store(),
        ));
        automations
            .add(send_message_automation(
                "reply",
                Trigger::MessageIn {
                    sender: SenderKind::User,
                    conversation: None,
                },
            ))
            .unwrap();
        let task = automations.start();
        let mut events = bus.subscribe();

        bus.publish(message_in("tui", Sender::User("local".to_string())));

        let record_in_history = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if let Event::ToolRequest {
                    record_in_history, ..
                } = events.recv().await.expect("event")
                {
                    break record_in_history;
                }
            }
        })
        .await
        .expect("tool request");
        task.abort();

        assert!(!record_in_history);
    }

    #[tokio::test]
    async fn trigger_automation_tool_returns_after_dispatch() {
        let bus = Bus::new(16);
        let tools = test_tools(&bus);
        let automations = Arc::new(Automations::new(
            bus.clone(),
            Arc::clone(&tools.registry),
            temp_store(),
        ));
        automations
            .add(send_message_automation(
                "manual",
                Trigger::MessageIn {
                    sender: SenderKind::User,
                    conversation: None,
                },
            ))
            .unwrap();
        let task = automations.start();
        let context = ExecutionContext::new(InvocationScope::ScheduledAutomation {
            automation_id: "test".to_string(),
        });

        let result = crate::tools::run_tool(
            &tools,
            "trigger_automation",
            serde_json::json!({ "automation_id": "manual" }),
            &context,
        )
        .await;
        task.abort();

        assert!(result.unwrap()["execution_id"].as_str().is_some());
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
                "name": "later",
                "at": (chrono::Local::now() + chrono::Duration::hours(1)).to_rfc3339(),
                "tool": "send_message",
                "args": { "channel": "tui", "content": "first" },
            }),
            &context,
        )
        .await
        .unwrap();
        crate::tools::run_tool(
            &tools,
            "edit_automation",
            serde_json::json!({
                "name": "later",
                "at": (chrono::Local::now() + chrono::Duration::hours(2)).to_rfc3339(),
                "tool": "send_message",
                "args": { "channel": "tui", "content": "second" },
            }),
            &context,
        )
        .await
        .unwrap();

        let stored = store.load_all().unwrap();
        assert_eq!(stored.len(), 1);
        let AutomationEffect::Tool { args, .. } = &stored[0].effect else {
            panic!("expected tool automation");
        };
        assert_eq!(args["content"], "second");
        task.abort();
    }

    #[tokio::test]
    async fn agent_task_invocations_run_in_order() {
        let bus = Bus::new(16);
        let tools = test_tools(&bus);
        let automations = Arc::new(Automations::new(
            bus.clone(),
            Arc::clone(&tools.registry),
            temp_store(),
        ));
        automations.add(agent_task_automation("serial")).unwrap();
        let task = automations.start();
        let mut events = bus.subscribe();
        let context = ExecutionContext::new(InvocationScope::ScheduledAutomation {
            automation_id: "serial".to_string(),
        });
        let first = automations.dispatch("serial", context.clone(), serde_json::json!({}));
        let second = automations.dispatch("serial", context, serde_json::json!({}));

        let request = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if let Event::AgentRequest { execution_id, .. } =
                    events.recv().await.expect("event")
                {
                    break execution_id;
                }
            }
        })
        .await
        .expect("first agent request");
        assert_eq!(request, first);

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), async {
                loop {
                    if matches!(
                        events.recv().await.expect("event"),
                        Event::AgentRequest { .. }
                    ) {
                        break;
                    }
                }
            })
            .await
            .is_err()
        );

        bus.publish(Event::AgentResult {
            execution_id: first,
            result: Ok(None),
        });
        let request = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if let Event::AgentRequest { execution_id, .. } =
                    events.recv().await.expect("event")
                {
                    break execution_id;
                }
            }
        })
        .await
        .expect("second agent request");
        task.abort();

        assert_eq!(request, second);
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
            .add(send_message_automation("once", Trigger::At { datetime }))
            .unwrap();
        let task = automations.start();
        let mut events = bus.subscribe();

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if matches!(
                    events.recv().await.expect("event"),
                    Event::AutomationInvocation { .. }
                ) {
                    break;
                }
            }
        })
        .await
        .expect("automation invocation");

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
            .add(send_message_automation(
                "daily-news",
                Trigger::MessageIn {
                    sender: SenderKind::User,
                    conversation: None,
                },
            ))
            .unwrap();

        automations.delete("daily-news").unwrap();

        assert!(!automations.contains("daily-news"));
        assert!(store.load_all().unwrap().is_empty());
    }

    #[test]
    fn delete_allows_builtin_automation() {
        let bus = Bus::new(4);
        let tools = test_tools(&bus);
        let automations = Arc::new(Automations::new(
            bus.clone(),
            Arc::clone(&tools.registry),
            temp_store(),
        ));
        automations.add(Automations::handle_conversation()).unwrap();

        let result = automations.delete("handle-conversation");

        assert!(result.is_ok());
        assert!(!automations.contains("handle-conversation"));
    }
}
