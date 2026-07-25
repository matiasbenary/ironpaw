mod event;

pub use event::{
    AgentRequest, ApiKeyError, AutomationAction, AutomationEffectSpec, AutomationSpec,
    AutomationTriggerSpec, ChannelAction, ConversationKey, Event, ExecutionContext,
    InvocationScope, MessageRef, Sender, SenderKind, UserInputOption, UserInputRequest,
};
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct Bus {
    tx: broadcast::Sender<Event>,
}

impl Bus {
    pub fn new(buffer: usize) -> Self {
        let (tx, _) = broadcast::channel(buffer);
        Self { tx }
    }

    pub fn publish(&self, event: Event) {
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }
}
