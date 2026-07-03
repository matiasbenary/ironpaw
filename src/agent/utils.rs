use crate::{
    bus::{Bus, ChannelAction, ConversationKey, Event, Sender},
    message::Message,
};

pub(super) fn publish_progress(bus: &Bus, conversation: &ConversationKey, content: String) {
    bus.publish(Event::MessageOut {
        channel: conversation.channel.clone(),
        recipient: Sender::User,
        conversation: Some(conversation.clone()),
        message: Message::Assistant {
            text: Some(content),
            attachments: Vec::new(),
            tool_calls: Vec::new(),
        },
    });
}

pub(super) fn publish_channel_action(
    bus: &Bus,
    conversation: &ConversationKey,
    action: ChannelAction,
) {
    bus.publish(Event::ChannelAction {
        channel: conversation.channel.clone(),
        recipient: Sender::User,
        conversation: conversation.clone(),
        action,
    });
}
