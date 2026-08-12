use crate::{
    bus::ConversationKey,
    history::{History, HistoryEntry, MessageMetadata},
    message::Message,
};

pub(super) fn append_channel_history(
    history: &dyn History,
    conversation: &ConversationKey,
    external_id: Option<String>,
    message: &Message,
) -> Result<(), String> {
    history.append(
        conversation,
        &[HistoryEntry {
            message: message.clone(),
            metadata: MessageMetadata::new(conversation.clone(), external_id),
        }],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bus::MessageRef, history::SqliteHistory};

    fn temp_history() -> SqliteHistory {
        let path = std::env::temp_dir().join(format!(
            "ironpaw-bridge-history-{}.db",
            uuid::Uuid::new_v4()
        ));
        SqliteHistory::new(path).unwrap()
    }

    #[test]
    fn delete_history_removes_matching_channel_message_at_boundary() {
        let history = temp_history();
        let conversation = ConversationKey {
            channel: "telegram".to_string(),
            chat_id: "chat".to_string(),
            thread_id: None,
        };
        let message = Message::User {
            text: "secret".to_string(),
            attachments: Vec::new(),
        };
        append_channel_history(
            &history,
            &conversation,
            Some("provider-1".to_string()),
            &message,
        )
        .unwrap();

        history
            .delete_external_message(&MessageRef {
                conversation: conversation.clone(),
                message_id: "provider-1".to_string(),
            })
            .unwrap();

        assert!(history.load(&conversation).unwrap().is_empty());
    }
}
