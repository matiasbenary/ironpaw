use crate::message::{Message, ToolCall};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Sender {
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
    pub depth: usize,
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
        #[serde(default)]
        sender: Option<Sender>,
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
        channel: String,
        sender: Sender,
        #[serde(default)]
        channel_user_id: Option<String>,
        conversation: ConversationKey,
        #[serde(default)]
        message_id: Option<String>,
        message: Message,
    },
    MessageOut {
        channel: String,
        recipient: Sender,
        #[serde(default)]
        conversation: Option<ConversationKey>,
        message: Message,
    },
    DeleteMessage {
        channel: String,
        message: MessageRef,
    },
    ChannelAction {
        channel: String,
        recipient: Sender,
        conversation: ConversationKey,
        action: ChannelAction,
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
