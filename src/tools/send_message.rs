use async_trait::async_trait;

use crate::{
    bus::{Bus, ChannelAddress, Event, Sender},
    openai::{ChatCompletionTools, assistant_response_message},
    tools::{self, InternalTool},
};

#[derive(Clone)]
pub struct SendMessageTool {
    bus: Bus,
}

impl SendMessageTool {
    pub fn new(bus: Bus) -> Self {
        Self { bus }
    }

    pub async fn execute(&self, params: SendMessageParams) -> Result<SendMessageOutput, String> {
        self.bus.publish(Event::MessageOut {
            channel: params.channel.clone(),
            recipient: Sender::User,
            address: params.address(),
            message: assistant_response_message(params.content),
        });

        Ok(SendMessageOutput {
            ok: true,
            channel: params.channel,
        })
    }
}

#[async_trait]
impl InternalTool for SendMessageTool {
    fn name(&self) -> &'static str {
        "send_message"
    }

    fn openai_schema(&self) -> ChatCompletionTools {
        tools::openai_tool_schema::<SendMessageParams>(
            self.name(),
            "Send a message to the user through one of Warden's channels.",
        )
    }

    async fn execute(&self, args: serde_json::Value) -> Result<serde_json::Value, String> {
        tools::execute_typed(args, |params| SendMessageTool::execute(self, params)).await
    }
}

#[derive(schemars::JsonSchema, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendMessageParams {
    #[schemars(description = "The channel to send through, for example telegram or tui.")]
    pub channel: String,
    #[schemars(description = "The message text to send.")]
    pub content: String,
    #[serde(default)]
    #[schemars(
        description = "Optional channel conversation id. If omitted, Warden uses the channel default."
    )]
    pub conversation_id: Option<String>,
    #[serde(default)]
    #[schemars(description = "Optional channel thread/topic id.")]
    pub thread_id: Option<String>,
}

impl SendMessageParams {
    fn address(&self) -> Option<ChannelAddress> {
        self.conversation_id
            .as_ref()
            .map(|conversation_id| ChannelAddress {
                channel: self.channel.clone(),
                conversation_id: conversation_id.clone(),
                thread_id: self.thread_id.clone(),
            })
    }
}

#[derive(serde::Serialize)]
pub struct SendMessageOutput {
    ok: bool,
    channel: String,
}
