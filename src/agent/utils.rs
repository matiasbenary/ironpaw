use crate::bus::{Bus, ChannelAction, ConversationKey, Event};

pub(super) fn publish_channel_action(
    bus: &Bus,
    conversation: &ConversationKey,
    action: ChannelAction,
) {
    bus.publish(Event::ChannelAction {
        conversation: conversation.clone(),
        action,
    });
}
