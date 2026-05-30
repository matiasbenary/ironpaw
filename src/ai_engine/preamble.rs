use crate::{
    bus::ChannelAddress,
    openai::{EasyInputContent, EasyInputMessage, InputItem, Role},
};

pub(super) fn system_preamble(channel: &str, address: &ChannelAddress) -> Vec<InputItem> {
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

    vec![
        EasyInputMessage {
            r#type: Default::default(),
            role: Role::System,
            content: EasyInputContent::Text(text),
            phase: None,
        }
        .into(),
    ]
}

pub(super) fn files_shared_notice_input_item(count: usize, channel: &str) -> InputItem {
    EasyInputMessage {
        r#type: Default::default(),
        role: Role::System,
        content: EasyInputContent::Text(format!(
            "{count} file(s) automatically shared with the user on channel '{channel}'."
        )),
        phase: None,
    }
    .into()
}
