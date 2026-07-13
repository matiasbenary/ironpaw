use crate::message::{Message, ToolCall};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Sender {
    User(String),
    Guest(String),
    System,
}

impl Sender {
    pub fn kind(&self) -> SenderKind {
        match self {
            Sender::User(_) => SenderKind::User,
            Sender::Guest(_) => SenderKind::Guest,
            Sender::System => SenderKind::System,
        }
    }

    pub fn channel_user_id(&self) -> Option<&str> {
        match self {
            Sender::User(channel_user_id) | Sender::Guest(channel_user_id) => Some(channel_user_id),
            Sender::System => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SenderKind {
    User,
    Guest,
    System,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationKey {
    pub channel: String,
    pub chat_id: String,
    #[serde(default)]
    pub thread_id: Option<String>,
}

impl ConversationKey {
    pub fn storage_key(&self) -> String {
        match &self.thread_id {
            Some(thread_id) => format!("{}/{}/{}", self.channel, self.chat_id, thread_id),
            None => format!("{}/{}", self.channel, self.chat_id),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageRef {
    pub conversation: ConversationKey,
    pub message_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutionContext {
    pub invoked_by: InvocationScope,
    /// Every registered tool entered through the host runner, in call order.
    pub tool_stack: Vec<String>,
    pub depth: usize,
}

impl ExecutionContext {
    pub fn new(invoked_by: InvocationScope) -> Self {
        Self {
            invoked_by,
            tool_stack: Vec::new(),
            depth: 0,
        }
    }

    pub fn with_tool(&self, tool_name: String) -> Self {
        let mut tool_stack = self.tool_stack.clone();
        tool_stack.push(tool_name);
        Self {
            invoked_by: self.invoked_by.clone(),
            tool_stack,
            depth: self.depth,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InvocationScope {
    Conversation(ConversationKey),
    ScheduledAutomation { automation_id: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelAction {
    #[serde(rename = "typing")]
    Typing,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInputRequest {
    pub question: String,
    pub options: Vec<UserInputOption>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInputOption {
    pub label: String,
    pub value: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AutomationSpec {
    pub name: String,
    pub trigger: AutomationTriggerSpec,
    pub effect: AutomationEffectSpec,
}

pub type AutomationEffectSpec = crate::automations::AutomationEffect;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AutomationTriggerSpec {
    At {
        datetime: String,
    },
    Cron {
        expression: String,
    },
    MessageIn {
        sender: SenderKind,
        #[serde(default)]
        conversation: Option<ConversationKey>,
    },
    ToolResult,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AutomationAction {
    Create { automation: AutomationSpec },
    Delete { name: String },
    Inspect { name: String },
    List,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Event {
    MessageIn {
        sender: Sender,
        conversation: ConversationKey,
        #[serde(default)]
        message_id: Option<String>,
        message: Message,
    },
    MessageOut {
        channel: String,
        #[serde(default)]
        chat_id: Option<String>,
        #[serde(default)]
        thread_id: Option<String>,
        message: Message,
    },
    DeleteMessage {
        message: MessageRef,
    },
    ChannelAction {
        conversation: ConversationKey,
        action: ChannelAction,
    },
    UserInputRequested {
        conversation: ConversationKey,
        request: UserInputRequest,
    },
    Automation {
        id: String,
        action: AutomationAction,
    },
    AutomationResult {
        id: String,
        result: Result<serde_json::Value, String>,
    },
    ToolRequest {
        context: ExecutionContext,
        tool_call: ToolCall,
        args: serde_json::Value,
    },
    ToolResult {
        context: ExecutionContext,
        tool_call_id: String,
        tool_name: String,
        result: serde_json::Value,
    },
    #[serde(skip)]
    SecretsUnlocked,
}
