use std::{future::Future, pin::Pin, sync::Arc};

use tokio::sync::RwLock;

use crate::{
    bus::{Bus, ConversationKey, Event, MessageIn, Sender, SenderKind},
    history::{History, HistoryEntry, HistoryId, MessageMetadata},
    message::Message,
};

#[derive(Clone)]
pub(crate) struct Bridge {
    bus: Bus,
    history: Arc<dyn History>,
    channels: Arc<RwLock<Vec<Arc<dyn Channel>>>>,
}

pub(crate) struct InboundMessage {
    pub sender: Sender,
    pub conversation: ConversationKey,
    pub message_id: Option<String>,
    pub message: Message,
}

pub(crate) struct Delivery {
    pub conversation: Option<ConversationKey>,
    pub external_id: Option<String>,
}

pub(crate) type DeliveryFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Delivery, String>> + Send + 'a>>;

pub(crate) trait Channel: Send + Sync {
    fn name(&self) -> &str;
    fn deliver<'a>(&'a self, event: &'a Event) -> DeliveryFuture<'a>;
}

impl Bridge {
    pub(crate) fn new(bus: Bus, history: Arc<dyn History>) -> Self {
        Self {
            bus,
            history,
            channels: Arc::default(),
        }
    }

    pub(crate) async fn add_channel(&self, channel: Arc<dyn Channel>) {
        let mut channels = self.channels.write().await;
        channels.retain(|existing| existing.name() != channel.name());
        channels.push(channel);
    }

    pub(crate) async fn remove_channel(&self, name: &str) -> bool {
        let mut channels = self.channels.write().await;
        let before = channels.len();
        channels.retain(|channel| channel.name() != name);
        channels.len() != before
    }

    pub(crate) fn receive(&self, inbound: InboundMessage) -> Result<(), String> {
        if !matches!(inbound.message, Message::User { .. }) {
            return Err("inbound channel message must be a user message".to_string());
        }
        if api_key_change_requested(&inbound.sender, &inbound.message) {
            self.bus.publish(Event::ApiKeyChangeRequested {
                conversation: inbound.conversation,
            });
            return Ok(());
        }
        let metadata =
            MessageMetadata::new(inbound.conversation.clone(), inbound.message_id.clone());
        let mut entries = vec![HistoryEntry {
            message: inbound.message.clone(),
            metadata: metadata.clone(),
        }];
        if let Message::User { attachments, .. } = &inbound.message
            && !attachments.is_empty()
        {
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
            entries.push(HistoryEntry {
                message: Message::System(format!(
                    "[attached files]\n{files}\nUse inspect_file with the filename to inspect an attached file."
                )),
                metadata,
            });
        }
        self.history.append(
            &HistoryId::Conversation(inbound.conversation.clone()),
            &entries,
        )?;
        let Message::User { text, attachments } = inbound.message else {
            unreachable!("inbound message type was validated")
        };
        self.bus.publish(Event::MessageIn(MessageIn {
            sender: inbound.sender,
            conversation: inbound.conversation,
            message_id: inbound.message_id,
            text,
            attachments,
        }));
        Ok(())
    }

    pub(crate) async fn run(&self) {
        let mut events = self.bus.subscribe();
        loop {
            let event = match events.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            };
            let Some(channel_name) = event_channel(&event) else {
                continue;
            };
            let channel = self
                .channels
                .read()
                .await
                .iter()
                .find(|channel| channel.name() == channel_name)
                .cloned();
            let Some(channel) = channel else {
                continue;
            };
            if let Event::DeleteMessage { message } = &event
                && let Err(error) = self.history.delete_external_message(message)
            {
                eprintln!("failed to delete {channel_name} history entry: {error}");
            }
            match channel.deliver(&event).await {
                Ok(delivery) => self.persist_delivery(channel_name, &event, delivery),
                Err(error) => eprintln!("{channel_name} outbound event failed: {error}"),
            }
        }
    }

    fn persist_delivery(&self, channel: &str, event: &Event, delivery: Delivery) {
        let (Event::MessageOut { message, .. }, Some(conversation)) =
            (event, delivery.conversation)
        else {
            return;
        };
        if let Err(error) = self.history.append(
            &HistoryId::Conversation(conversation.clone()),
            &[HistoryEntry {
                message: message.clone(),
                metadata: MessageMetadata::new(conversation.clone(), delivery.external_id),
            }],
        ) {
            eprintln!("failed to persist {channel} outbound history: {error}");
        }
    }
}

fn event_channel(event: &Event) -> Option<&str> {
    match event {
        Event::MessageOut { channel, .. } => Some(channel),
        Event::DeleteMessage { message } => Some(&message.conversation.channel),
        Event::ChannelAction { conversation, .. }
        | Event::UserInputRequested { conversation, .. } => Some(&conversation.channel),
        _ => None,
    }
}

fn api_key_change_requested(sender: &Sender, message: &Message) -> bool {
    if sender.kind() != SenderKind::User {
        return false;
    }
    let Message::User { text, attachments } = message else {
        return false;
    };
    attachments.is_empty()
        && text.trim().split_whitespace().count() == 1
        && text.trim().split_whitespace().next() == Some("/change_api_key")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_owner_can_request_api_key_change() {
        let command = Message::User {
            text: "/change_api_key".to_string(),
            attachments: Vec::new(),
        };
        assert!(api_key_change_requested(
            &Sender::User("owner".to_string()),
            &command
        ));
        assert!(!api_key_change_requested(
            &Sender::Guest("guest".to_string()),
            &command
        ));
    }

    #[test]
    fn api_key_change_command_does_not_accept_a_key_argument() {
        let command = Message::User {
            text: "/change_api_key secret".to_string(),
            attachments: Vec::new(),
        };
        assert!(!api_key_change_requested(
            &Sender::User("owner".to_string()),
            &command
        ));
    }
}
