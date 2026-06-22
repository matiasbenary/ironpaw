use crate::message::Message;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Sender {
    User,
    Guest,
    System,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelAddress {
    pub channel: String,
    pub conversation_id: String,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub message_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelAction {
    #[serde(rename = "typing")]
    Typing,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AgentRequest {
    RunTask { task: String },
    ContinueConversation { conversation_id: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AutomationSpec {
    pub name: String,
    pub trigger: AutomationTriggerSpec,
    pub tool: String,
    pub args: serde_json::Value,
}

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
        channel: Option<String>,
    },
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
        address: ChannelAddress,
        message: Message,
    },
    MessageOut {
        channel: String,
        recipient: Sender,
        #[serde(default)]
        address: Option<ChannelAddress>,
        message: Message,
    },
    DeleteMessage {
        channel: String,
        address: ChannelAddress,
    },
    ChannelAction {
        channel: String,
        recipient: Sender,
        address: ChannelAddress,
        action: ChannelAction,
    },
    AgentRequest {
        request_id: String,
        #[serde(default)]
        destination: Option<(String, ChannelAddress)>,
        request: AgentRequest,
    },
    AgentResponse {
        request_id: String,
        response: Result<Option<String>, String>,
    },
    Automation {
        id: String,
        action: AutomationAction,
    },
    AutomationResult {
        id: String,
        result: Result<serde_json::Value, String>,
    },
}
