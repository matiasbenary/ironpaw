use crate::{
    message::{Attachment, Message, ToolCall},
    tools::ExecutionContext,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "channel_user_id", rename_all = "snake_case")]
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, schemars::JsonSchema, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SenderKind {
    User,
    Guest,
    System,
}

#[derive(Clone, Debug, PartialEq, Eq, schemars::JsonSchema, Serialize, Deserialize)]
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MessageIn {
    pub sender: Sender,
    pub conversation: ConversationKey,
    #[serde(default)]
    pub message_id: Option<String>,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageRef {
    pub conversation: ConversationKey,
    pub message_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelAction {
    #[serde(rename = "typing")]
    Typing,
    #[serde(rename = "progress")]
    Progress { text: String },
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
pub enum AutomationAction {
    Create {
        automation: crate::automations::Automation,
    },
    Edit {
        automation: crate::automations::Automation,
    },
    Delete {
        name: String,
    },
    Inspect {
        name: String,
    },
    List,
    Trigger {
        name: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AgentRequest {
    ContinueConversation {
        conversation: ConversationKey,
        context: ExecutionContext,
    },
    Task {
        automation_id: String,
        prompt: String,
        tools: Vec<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Event {
    MessageIn(MessageIn),
    MessageOut {
        channel: String,
        #[serde(default)]
        chat_id: Option<String>,
        #[serde(default)]
        thread_id: Option<String>,
        #[serde(default)]
        message_id: Option<String>,
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
    AgentRequest {
        execution_id: String,
        request: AgentRequest,
    },
    AgentResult {
        execution_id: String,
        result: Result<Option<String>, String>,
    },
    ApiKeyChangeRequested {
        conversation: ConversationKey,
    },
    ToolRequest {
        request_id: String,
        context: ExecutionContext,
        tool_calls: Vec<ToolCall>,
        #[serde(default)]
        allowed_tool_names: Option<Vec<String>>,
    },
    ToolResponse {
        request_id: String,
        tool_call_ids: Vec<String>,
        result: Result<Vec<crate::tools::ToolCallResult>, String>,
    },
    ToolResult {
        request_id: String,
        tool_call_id: String,
        tool_name: String,
        result: crate::tools::ToolResult,
    },
    #[serde(skip)]
    SecretsUnlocked,
}
