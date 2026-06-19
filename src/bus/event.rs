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
    CronTick {
        schedule: String,
    },
}
