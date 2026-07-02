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
        channel: Option<String>,
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
    Automation {
        id: String,
        action: AutomationAction,
    },
    AutomationResult {
        id: String,
        result: Result<serde_json::Value, String>,
    },
    ToolRequest {
        origin: ToolOrigin,
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
    },
    ToolResult {
        origin: ToolOrigin,
        tool_call_id: String,
        tool_name: String,
        result: serde_json::Value,
    },
    #[serde(skip)]
    SecretsUnlocked,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ToolOrigin {
    Conversation {
        channel: String,
        sender: Sender,
        address: ChannelAddress,
    },
    Automation {
        automation_id: String,
    },
}
