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
            Your final message is returned to the caller that started the task. \
            Use file tools to persist state between executions when needed."
            .to_string(),
    };
    text.push_str(
        "\n\n## Tools\n\
        Use the callable tools provided with this request.\n\n\
        ## Automations\n\
        When the user requests a future or recurring action, create an automation. Build a \
        deterministic tool only for stateless fixed logic. Create a recurrent agent task when \
        the work needs persistent state, must stop or change its own automation, requires \
        judgment, or requires research.\n\n\
        ## Tips\n\
        Use focused tool calls to verify external facts or process user-provided data. Stop once \
        you have enough evidence to act; do not research broadly when it will not improve the result.",
    );

    vec![Message::System(text)]
}
