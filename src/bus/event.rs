use crate::openai::{ChatCompletionRequestMessage, ChatCompletionResponseMessage};
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
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Event {
    MessageIn {
        channel: String,
        sender: Sender,
        address: ChannelAddress,
        message: ChatCompletionRequestMessage,
    },
    MessageOut {
        channel: String,
        recipient: Sender,
        address: ChannelAddress,
        message: ChatCompletionResponseMessage,
    },
    CronTick {
        schedule: String,
    },
}
