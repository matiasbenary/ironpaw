use crate::{
    bus::{Bus, ChannelAddress, Event, Sender},
    message::Message,
};

pub(super) fn summarize_tool_args(arguments: &str) -> String {
    const MAX_LEN: usize = 600;
    let arguments = display_tool_args(arguments);
    if arguments.len() <= MAX_LEN {
        arguments
    } else {
        format!("{}\u{2026}", &arguments[..MAX_LEN])
    }
}

fn display_tool_args(arguments: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) else {
        return arguments.to_string();
    };
    value.to_string()
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
        message: Message::Assistant {
            text: Some(content),
            attachments: Vec::new(),
            tool_calls: Vec::new(),
        },
    });
}
