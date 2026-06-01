use crate::{
    bus::{Bus, ChannelAddress, Event, Sender},
    message::OutboundMessage,
    openai::{FunctionCallOutput, FunctionCallOutputItemParam, InputItem, Item},
};

pub(super) fn tool_result_input_item(tool_call_id: &str, result: impl AsRef<str>) -> InputItem {
    Item::from(FunctionCallOutputItemParam {
        call_id: tool_call_id.to_string(),
        output: FunctionCallOutput::Text(result.as_ref().to_string()),
        id: None,
        status: None,
    })
    .into()
}

pub(super) fn summarize_tool_args(arguments: &str) -> String {
    const MAX_LEN: usize = 600;
    if arguments.len() <= MAX_LEN {
        arguments.to_string()
    } else {
        format!("{}\u{2026}", &arguments[..MAX_LEN])
    }
}

pub(super) fn fill_current_destination(
    args: &mut serde_json::Value,
    progress: Option<(&Bus, &str, &ChannelAddress)>,
) {
    let Some((_bus, channel, address)) = progress else {
        return;
    };
    let Some(object) = args.as_object_mut() else {
        return;
    };
    if object.get("channel").is_some() {
        return;
    }
    object.insert(
        "channel".to_string(),
        serde_json::Value::String(channel.to_string()),
    );
    object
        .entry("conversation_id")
        .or_insert_with(|| serde_json::Value::String(address.conversation_id.clone()));
    if let Some(thread_id) = &address.thread_id {
        object
            .entry("thread_id")
            .or_insert_with(|| serde_json::Value::String(thread_id.clone()));
    }
}

pub(super) fn publish_progress(
    bus: &Bus,
    channel: &str,
    address: &ChannelAddress,
    content: String,
) {
    bus.publish(Event::MessageOut {
        channel: channel.to_string(),
        recipient: Sender::User,
        address: Some(address.clone()),
        message: OutboundMessage {
            content: Some(content),
            attachments: Vec::new(),
        },
    });
}
