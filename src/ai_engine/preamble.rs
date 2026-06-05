use crate::{bus::ChannelAddress, message::Message};

pub(super) fn system_preamble(channel: &str, address: &ChannelAddress) -> Vec<Message> {
    let mut text = format!(
        "You are a personal assistant. \
        The user is communicating with you through the '{channel}' channel \
        (conversation_id: '{}'",
        address.conversation_id,
    );
    if let Some(thread_id) = &address.thread_id {
        text.push_str(&format!(", thread_id: '{thread_id}'"));
    }
    text.push(')');
    text.push('.');

    vec![Message::System(text)]
}
