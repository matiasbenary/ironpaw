use crate::{
    bus::{Bus, ChannelAction, ChannelAddress, Event, Sender},
    message::Message,
};

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

pub(super) fn publish_channel_action(
    bus: &Bus,
    channel: &str,
    address: &ChannelAddress,
    action: ChannelAction,
) {
    bus.publish(Event::ChannelAction {
        channel: channel.to_string(),
        recipient: Sender::User,
        address: address.clone(),
        action,
    });
}
