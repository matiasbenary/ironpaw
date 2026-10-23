use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use crate::{bus::ConversationKey, message::Message};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ExecutionContext {
    /// Host-owned path relative to the runtime files directory.
    pub workspace: PathBuf,
    tool_workspace: Option<PathBuf>,
    /// Every registered tool entered through the host runner, in call order.
    pub tool_stack: Vec<String>,
    /// Tool usage for this turn, shared by nested and asynchronous tools.
    #[serde(skip)]
    pub(crate) tool_budget: Arc<Mutex<ToolBudget>>,
    pub depth: usize,
    /// Conversation this run belongs to, when there is one to ask for approvals.
    #[serde(default)]
    pub conversation: Option<ConversationKey>,
}

#[derive(Debug, Default)]
pub(crate) struct ToolBudget {
    pub used: usize,
    pub pending_warnings: Vec<Message>,
}

impl ExecutionContext {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            tool_workspace: None,
            tool_stack: Vec::new(),
            tool_budget: Arc::new(Mutex::new(ToolBudget::default())),
            depth: 0,
            conversation: None,
        }
    }

    pub fn with_conversation(mut self, conversation: ConversationKey) -> Self {
        self.conversation = Some(conversation);
        self
    }

    pub fn with_tool(&self, tool_name: String) -> Self {
        let mut tool_stack = self.tool_stack.clone();
        tool_stack.push(tool_name);
        Self {
            workspace: self.workspace.clone(),
            tool_workspace: self.tool_workspace.clone(),
            tool_stack,
            tool_budget: Arc::clone(&self.tool_budget),
            depth: self.depth,
            conversation: self.conversation.clone(),
        }
    }

    pub(crate) fn with_tool_workspace(&self, workspace: PathBuf) -> Self {
        Self {
            workspace: self.workspace.clone(),
            tool_workspace: Some(workspace),
            tool_stack: self.tool_stack.clone(),
            tool_budget: Arc::clone(&self.tool_budget),
            depth: self.depth,
            conversation: self.conversation.clone(),
        }
    }

    pub(crate) fn files_workspace(&self) -> &std::path::Path {
        self.tool_workspace.as_deref().unwrap_or(&self.workspace)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_external_tools_are_siblings() {
        let context = ExecutionContext::new(PathBuf::from("conversations/owner"));
        let first =
            context.with_tool_workspace(crate::message::external_tool_workspace(&context, "first"));
        let second =
            first.with_tool_workspace(crate::message::external_tool_workspace(&first, "second"));

        assert_eq!(
            first.files_workspace(),
            std::path::Path::new("conversations/owner/tools/6669727374")
        );
        assert_eq!(
            second.files_workspace(),
            std::path::Path::new("conversations/owner/tools/7365636f6e64")
        );
    }

    #[test]
    fn nested_tool_contexts_keep_the_conversation_for_approvals() {
        let conversation = ConversationKey {
            channel: "local-client".to_string(),
            chat_id: "chat".to_string(),
            thread_id: None,
        };
        let context = ExecutionContext::new(PathBuf::from("conversations/owner"))
            .with_conversation(conversation.clone());

        let nested = context
            .with_tool("script".to_string())
            .with_tool_workspace(PathBuf::from("tools/x"))
            .with_tool("outlayer".to_string());

        assert_eq!(nested.conversation, Some(conversation));
    }
}
