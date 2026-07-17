use crate::{
    bus::{Bus, ChannelAction, ConversationKey, Event},
    message::Message,
};

pub(super) fn publish_progress(bus: &Bus, conversation: &ConversationKey, content: String) {
    bus.publish(Event::MessageOut {
        channel: conversation.channel.clone(),
        chat_id: Some(conversation.chat_id.clone()),
        thread_id: conversation.thread_id.clone(),
        message_id: None,
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
        conversation: conversation.clone(),
        action,
    });
}
