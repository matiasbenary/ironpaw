use std::sync::Arc;

use crate::{
    bus::{AgentRequest, Bus, Event},
    message::Message,
};

use super::Agent;

pub struct AgentHandler {
    bus: Bus,
    agent: Arc<Agent>,
}

impl AgentHandler {
    pub fn new(bus: Bus, agent: Arc<Agent>) -> Self {
        Self { bus, agent }
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
                    }) => this.spawn_agent_run(execution_id, request),
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }

    fn spawn_agent_run(&self, execution_id: String, request: AgentRequest) {
        let bus = self.bus.clone();
        let agent = Arc::clone(&self.agent);
        tokio::spawn(async move {
            let result = match request {
                AgentRequest::ContinueConversation {
                    conversation,
                    context,
                } => {
                    let result = agent
                        .handle_conversation(Some(&conversation), context)
                        .await;
                    if let Ok(Some(text)) = &result {
                        bus.publish(Event::MessageOut {
                            channel: conversation.channel.clone(),
                            chat_id: Some(conversation.chat_id.clone()),
                            thread_id: conversation.thread_id.clone(),
                            message: Message::Assistant {
                                text: Some(text.clone()),
                                attachments: Vec::new(),
                                tool_calls: Vec::new(),
                            },
                        });
                    }
                    result
                }
                AgentRequest::Task {
                    automation_id,
                    prompt,
                    tools,
                } => agent.run_task(automation_id, prompt, tools).await,
            };
            bus.publish(Event::AgentResult {
                execution_id,
                result,
            });
        });
    }
}
