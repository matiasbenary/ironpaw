use crate::{
    bus::ConversationKey,
    history::{History, HistoryEntry, MessageMetadata},
    message::{Attachment, Message},
};

/// Store a user message received by a channel with the filenames it can inspect.
pub fn append_inbound_message(
    history: &dyn History,
    conversation: &ConversationKey,
    external_id: Option<String>,
    message: &Message,
) -> Result<(), String> {
    let metadata = MessageMetadata::new(conversation.clone(), external_id);
    let mut entries = vec![HistoryEntry {
        message: message.clone(),
        metadata: metadata.clone(),
    }];
    if let Message::User { attachments, .. } = message
        && !attachments.is_empty()
    {
        entries.push(HistoryEntry {
            message: attachment_context(attachments),
            metadata,
        });
    }
    history.append(conversation, &entries)
}

fn attachment_context(attachments: &[Attachment]) -> Message {
    let files = attachments
        .iter()
        .map(|attachment| {
            format!(
                "- filename: {}\n  mime_type: {}",
                attachment.filename, attachment.mime_type,
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Message::System(format!(
        "[attached files]\n{files}\nUse inspect_file with the filename to inspect an attached file."
    ))
}
