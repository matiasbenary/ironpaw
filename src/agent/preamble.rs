use super::Destination;
use crate::message::Message;

pub(super) fn system_preamble(destination: Destination<'_>) -> Vec<Message> {
    let mut text = match destination {
        Some(conversation) => {
            let mut text = format!(
                "You are a personal assistant. \
                The user is communicating with you through the '{}' channel \
                (chat_id: '{}'",
                conversation.channel, conversation.chat_id,
            );
            if let Some(thread_id) = &conversation.thread_id {
                text.push_str(&format!(", thread_id: '{thread_id}'"));
            }
            text.push(')');
            text.push('.');
            text.push_str(
                " Your final message is delivered to this conversation \
                automatically: do not use send_message to reply here, only \
                to reach a different channel or conversation.",
            );
            text
        }
        None => "You are a personal assistant running a background task. \
            Your final message is returned to the caller that started the task."
            .to_string(),
    };
    text.push_str(&format!(
        " The current local time is {}.",
        chrono::Local::now().format("%A %Y-%m-%dT%H:%M:%S%:z")
    ));
    text.push_str(
        " When the user requests a future or recurring action, create an automation. \
        Build a deterministic tool for fixed logic, or create a recurrent agent task when \
        the work requires judgment or research.",
    );
    text.push_str(
        " Use focused tool calls to verify external facts or process user-provided data. \
        Be mindful of unnecessary tool calls. Stop once you have enough evidence to act; do not research broadly when it will not improve the result.",
    );

    vec![Message::System(text)]
}
