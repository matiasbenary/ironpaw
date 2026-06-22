use crate::{
    bus::ChannelAddress,
    history::{History, HistoryEntry, MessageMetadata},
    message::Message,
};

pub(super) fn append_channel_history(
    history: &dyn History,
    channel: &str,
    address: &ChannelAddress,
    external_id: Option<String>,
    message: &Message,
) {
    history.append(
        &address.conversation_id,
        &[HistoryEntry {
            message: message.clone(),
            metadata: MessageMetadata::new(
                channel.to_string(),
                address.conversation_id.clone(),
                address.thread_id.clone(),
                external_id,
            ),
        }],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::SqliteHistory;

    fn temp_history() -> SqliteHistory {
        let path =
            std::env::temp_dir().join(format!("warden-bridge-history-{}.db", uuid::Uuid::new_v4()));
        SqliteHistory::new(path).unwrap()
    }

    #[test]
    fn inbound_history_is_stored_at_channel_boundary() {
        let history = temp_history();
        let address = ChannelAddress {
            channel: "telegram".to_string(),
            conversation_id: "chat".to_string(),
            thread_id: Some("topic".to_string()),
            message_id: Some("provider-1".to_string()),
        };
        let message = Message::User {
            text: "secret".to_string(),
            attachments: Vec::new(),
        };

        append_channel_history(
            &history,
            "telegram",
            &address,
            address.message_id.clone(),
            &message,
        );

        let entries = history.load("chat");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].metadata.channel, "telegram");
        assert_eq!(entries[0].metadata.channel_conversation_id, "chat");
        assert_eq!(entries[0].metadata.thread_id.as_deref(), Some("topic"));
        assert_eq!(
            entries[0].metadata.external_id.as_deref(),
            Some("provider-1")
        );
        assert_eq!(entries[0].message, message);
    }

    #[test]
    fn outbound_history_is_stored_at_channel_boundary() {
        let history = temp_history();
        let address = ChannelAddress {
            channel: "telegram".to_string(),
            conversation_id: "chat".to_string(),
            thread_id: Some("topic".to_string()),
            message_id: None,
        };
        let message = Message::Assistant {
            text: Some("hi".to_string()),
            attachments: Vec::new(),
            tool_calls: Vec::new(),
        };

        append_channel_history(&history, "telegram", &address, None, &message);

        let entries = history.load("chat");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].metadata.channel, "telegram");
        assert_eq!(entries[0].metadata.channel_conversation_id, "chat");
        assert_eq!(entries[0].metadata.thread_id.as_deref(), Some("topic"));
        assert_eq!(entries[0].metadata.external_id, None);
        assert_eq!(entries[0].message, message);
    }

    #[test]
    fn delete_history_removes_matching_channel_message_at_boundary() {
        let history = temp_history();
        let address = ChannelAddress {
            channel: "telegram".to_string(),
            conversation_id: "chat".to_string(),
            thread_id: None,
            message_id: Some("provider-1".to_string()),
        };
        let message = Message::User {
            text: "secret".to_string(),
            attachments: Vec::new(),
        };
        append_channel_history(
            &history,
            "telegram",
            &address,
            address.message_id.clone(),
            &message,
        );

        history.delete_external_message(
            "telegram",
            &address.conversation_id,
            address.thread_id.as_deref(),
            address.message_id.as_deref().unwrap(),
        );

        assert!(history.load("chat").is_empty());
    }
}
