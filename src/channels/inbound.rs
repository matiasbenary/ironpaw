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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::SqliteHistory;

    #[test]
    fn attachment_includes_inspection_context() {
        let history = SqliteHistory::new(
            std::env::temp_dir().join(format!("ironpaw-inbound-{}.db", uuid::Uuid::new_v4())),
        )
        .unwrap();
        let conversation = ConversationKey {
            channel: "local-client".to_string(),
            chat_id: "chat".to_string(),
            thread_id: None,
        };
        let message = Message::User {
            text: "What is this?".to_string(),
            attachments: vec![Attachment {
                filename: "dog.jpg".to_string(),
                mime_type: "image/jpeg".to_string(),
                source: "ignored".into(),
            }],
        };

        append_inbound_message(
            &history,
            &conversation,
            Some("provider-1".to_string()),
            &message,
        )
        .unwrap();

        let entries = history.load(&conversation).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[1].metadata.external_id.as_deref(),
            Some("provider-1")
        );
        assert_eq!(
            entries[1].message,
            Message::System(
                "[attached files]\n- filename: dog.jpg\n  mime_type: image/jpeg\nUse inspect_file with the filename to inspect an attached file."
                    .to_string()
            )
        );
    }
}
