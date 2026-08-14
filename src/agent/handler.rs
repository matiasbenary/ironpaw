use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

use crate::{
    bus::{AgentRequest, Bus, Event},
    message::Message,
};

use super::Agent;

pub struct AgentHandler {
    bus: Bus,
    agent: Arc<Agent>,
    task_queues: Mutex<HashMap<String, AgentTaskQueue>>,
}

#[derive(Default)]
struct AgentTaskQueue {
    running: bool,
    pending: VecDeque<PendingAgentTask>,
}

struct PendingAgentTask {
    execution_id: String,
    automation_id: String,
    prompt: String,
    tools: Vec<String>,
}

impl AgentHandler {
    pub fn new(bus: Bus, agent: Arc<Agent>) -> Self {
        Self {
            bus,
            agent,
            task_queues: Mutex::new(HashMap::new()),
        }
    }

    pub fn start(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = Arc::clone(self);
        let mut events = self.bus.subscribe();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(Event::AgentRequest {
                        execution_id,
                        request,
                    }) => this.handle_request(execution_id, request),
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }

    fn handle_request(self: &Arc<Self>, execution_id: String, request: AgentRequest) {
        match request {
            AgentRequest::ContinueConversation {
                conversation,
                context,
            } => self.spawn_conversation(execution_id, conversation, context),
            AgentRequest::Task {
                automation_id,
                prompt,
                tools,
            } => self.enqueue_task(PendingAgentTask {
                execution_id,
                automation_id,
                prompt,
                tools,
            }),
        }
    }

    fn spawn_conversation(
        &self,
        execution_id: String,
        conversation: crate::bus::ConversationKey,
        context: crate::tools::ExecutionContext,
    ) {
        let bus = self.bus.clone();
        let agent = Arc::clone(&self.agent);
        tokio::spawn(async move {
            let result = agent.handle_conversation(&conversation, context).await;
            let text = match &result {
                Ok(Some(text)) => Some(text.clone()),
                Ok(None) => None,
                Err(error) => Some(format!("I couldn't complete your request: {error}")),
            };
            if let Some(text) = text {
                bus.publish(Event::MessageOut {
                    channel: conversation.channel.clone(),
                    chat_id: Some(conversation.chat_id.clone()),
                    thread_id: conversation.thread_id.clone(),
                    message_id: None,
                    message: Message::Assistant {
                        text: Some(text),
                        attachments: Vec::new(),
                        tool_calls: Vec::new(),
                    },
                });
            }
            bus.publish(Event::AgentResult {
                execution_id,
                result,
            });
        });
    }

    fn enqueue_task(self: &Arc<Self>, task: PendingAgentTask) {
        let mut queues = self.task_queues.lock().expect("agent task queues lock");
        let queue = queues.entry(task.automation_id.clone()).or_default();
        if queue.running {
            queue.pending.push_back(task);
            return;
        }
        queue.running = true;
        drop(queues);
        self.spawn_task(task);
    }

    fn spawn_task(self: &Arc<Self>, task: PendingAgentTask) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let result = this
                .agent
                .run_task(task.automation_id.clone(), task.prompt, task.tools)
                .await;
            this.bus.publish(Event::AgentResult {
                execution_id: task.execution_id,
                result,
            });
            this.complete_task(&task.automation_id);
        });
    }

    fn complete_task(self: &Arc<Self>, automation_id: &str) {
        let mut queues = self.task_queues.lock().expect("agent task queues lock");
        let queue = queues.get_mut(automation_id).expect("running agent task");
        let next = queue.pending.pop_front();
        if next.is_none() {
            queues.remove(automation_id);
        }
        drop(queues);
        if let Some(task) = next {
            self.spawn_task(task);
        }
    }
}
