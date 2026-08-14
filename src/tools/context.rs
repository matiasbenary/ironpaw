use std::sync::{Arc, Mutex};

use crate::{bus::ConversationKey, message::Message, tools::ToolResult};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ExecutionContext {
    pub invoked_by: InvocationScope,
    /// Every registered tool entered through the host runner, in call order.
    pub tool_stack: Vec<String>,
    /// Tool usage for this turn, shared by nested and asynchronous tools.
    #[serde(skip)]
    pub(crate) tool_budget: Arc<Mutex<ToolBudget>>,
    pub depth: usize,
}

#[derive(Debug, Default)]
pub(crate) struct ToolBudget {
    pub used: usize,
    pub pending_warnings: Vec<Message>,
}

impl ExecutionContext {
    pub fn new(invoked_by: InvocationScope) -> Self {
        Self {
            invoked_by,
            tool_stack: Vec::new(),
            tool_budget: Arc::new(Mutex::new(ToolBudget::default())),
            depth: 0,
        }
    }

    pub fn with_tool(&self, tool_name: String) -> Self {
        let mut tool_stack = self.tool_stack.clone();
        tool_stack.push(tool_name);
        Self {
            invoked_by: self.invoked_by.clone(),
            tool_stack,
            tool_budget: Arc::clone(&self.tool_budget),
            depth: self.depth,
        }
    }

    pub fn conversation(&self) -> Option<&ConversationKey> {
        match &self.invoked_by {
            InvocationScope::Conversation(conversation) => Some(conversation),
            InvocationScope::ScheduledAutomation { .. } => None,
        }
    }

    pub(crate) fn drain_tool_budget_messages(&self) -> Vec<Message> {
        std::mem::take(
            &mut self
                .tool_budget
                .lock()
                .expect("tool budget lock")
                .pending_warnings,
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum InvocationScope {
    Conversation(ConversationKey),
    ScheduledAutomation { automation_id: String },
}

#[derive(Clone)]
pub struct ToolResponder {
    sender: Arc<Mutex<Option<tokio::sync::oneshot::Sender<ToolResult>>>>,
}

impl ToolResponder {
    pub(crate) fn pending() -> (Self, tokio::sync::oneshot::Receiver<ToolResult>) {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        (
            Self {
                sender: Arc::new(Mutex::new(Some(sender))),
            },
            receiver,
        )
    }

    pub fn respond(&self, result: ToolResult) {
        let Some(sender) = self.sender.lock().expect("tool response lock").take() else {
            eprintln!("tool responder received a duplicate response");
            return;
        };
        if sender.send(result).is_err() {
            eprintln!("tool response receiver was dropped");
        }
    }
}
