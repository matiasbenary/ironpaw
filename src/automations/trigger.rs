use std::str::FromStr;

use crate::bus::{ConversationKey, Event, InvocationScope, SenderKind};

#[derive(Clone, serde::Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Trigger {
    /// Recurring, standard cron expression, evaluated in local time.
    Cron { expression: String },
    /// One-shot, fires once at the given instant.
    At {
        #[serde(with = "datetime_serde")]
        datetime: chrono::DateTime<chrono::FixedOffset>,
    },
    /// Fires on inbound user/channel messages. `conversation: None` matches any conversation.
    MessageIn {
        sender: SenderKind,
        #[serde(default)]
        conversation: Option<ConversationKey>,
    },
    /// Fires when a conversation-owned tool request has finished.
    ToolResult,
    /// Fires when the conversation agent cannot authenticate with its API key.
    ApiKeyError,
}

mod datetime_serde {
    use super::parse_datetime;

    pub fn serialize<S>(
        datetime: &chrono::DateTime<chrono::FixedOffset>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&datetime.to_rfc3339())
    }

    pub fn deserialize<'de, D>(
        deserializer: D,
    ) -> Result<chrono::DateTime<chrono::FixedOffset>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let text = <String as serde::Deserialize>::deserialize(deserializer)?;
        parse_datetime(&text).map_err(serde::de::Error::custom)
    }
}

impl Trigger {
    /// Accepts the common 5-field cron form (min hour dom mon dow) as well
    /// as the 6/7-field form with seconds (and optional year).
    pub fn cron(expression: &str) -> Result<Self, String> {
        let expression = match expression.split_whitespace().count() {
            5 => format!("0 {expression}"),
            _ => expression.to_string(),
        };
        cron::Schedule::from_str(&expression)
            .map_err(|err| format!("invalid cron expression: {err}"))?;
        Ok(Trigger::Cron { expression })
    }

    /// Accepts RFC 3339 (`2026-06-11T09:00:00+02:00`); a timestamp without
    /// an offset is interpreted as local time. A timestamp in the recent
    /// past fires immediately. Only clearly-stale timestamps are rejected.
    pub fn at(datetime: &str) -> Result<Self, String> {
        const GRACE: chrono::Duration = chrono::Duration::minutes(5);
        let datetime = parse_datetime(datetime)?;
        if datetime <= chrono::Local::now() - GRACE {
            return Err(format!("'{datetime}' is in the past"));
        }
        Ok(Trigger::At { datetime })
    }

    pub(super) fn matches(&self, event: &Event) -> bool {
        match (self, event) {
            (
                Trigger::MessageIn {
                    sender,
                    conversation,
                },
                Event::MessageIn {
                    sender: event_sender,
                    conversation: event_conversation,
                    ..
                },
            ) => {
                *sender == event_sender.kind()
                    && conversation
                        .as_ref()
                        .is_none_or(|conversation| conversation == event_conversation)
            }
            (Trigger::ToolResult, Event::ToolResult { context, .. }) => {
                matches!(context.invoked_by, InvocationScope::Conversation(_))
            }
            (Trigger::ApiKeyError, Event::ApiKeyError { .. }) => true,
            _ => false,
        }
    }
}

pub(crate) fn parse_datetime(text: &str) -> Result<chrono::DateTime<chrono::FixedOffset>, String> {
    if let Ok(datetime) = chrono::DateTime::parse_from_rfc3339(text) {
        return Ok(datetime);
    }
    let naive = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S")
        .map_err(|err| format!("invalid datetime '{text}' (use RFC 3339): {err}"))?;
    naive
        .and_local_timezone(chrono::Local)
        .single()
        .map(|datetime| datetime.fixed_offset())
        .ok_or_else(|| format!("ambiguous local datetime '{text}'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bus::{ConversationKey, Event, Sender},
        message::Message,
    };

    fn message_in(channel: &str, sender: Sender) -> Event {
        message_in_conversation(
            ConversationKey {
                channel: channel.to_string(),
                chat_id: "c1".to_string(),
                thread_id: None,
            },
            sender,
        )
    }

    fn message_in_conversation(conversation: ConversationKey, sender: Sender) -> Event {
        Event::MessageIn {
            sender,
            conversation,
            message_id: None,
            message: Message::User {
                text: "hi".to_string(),
                attachments: Vec::new(),
            },
        }
    }

    #[test]
    fn default_message_in_trigger_matches_user_messages_from_any_channel() {
        let trigger = Trigger::MessageIn {
            sender: SenderKind::User,
            conversation: None,
        };

        assert!(trigger.matches(&message_in("tui", Sender::User("local".to_string()))));
        assert!(trigger.matches(&message_in("telegram", Sender::User("123".to_string()))));
        assert!(!trigger.matches(&message_in("telegram", Sender::Guest("123".to_string()))));
    }

    #[test]
    fn conversation_filter_restricts_conversation() {
        let conversation = ConversationKey {
            channel: "telegram".to_string(),
            chat_id: "c1".to_string(),
            thread_id: None,
        };
        let trigger = Trigger::MessageIn {
            sender: SenderKind::Guest,
            conversation: Some(conversation),
        };
        assert!(trigger.matches(&message_in("telegram", Sender::Guest("123".to_string()))));
        assert!(!trigger.matches(&message_in_conversation(
            ConversationKey {
                channel: "telegram".to_string(),
                chat_id: "c2".to_string(),
                thread_id: None,
            },
            Sender::Guest("123".to_string()),
        )));
        assert!(!trigger.matches(&message_in("tui", Sender::User("local".to_string()))));
    }

    #[test]
    fn cron_trigger_accepts_standard_forms_and_rejects_invalid_input() {
        assert!(Trigger::cron("0 0 9 * * * *").is_ok());
        assert!(Trigger::cron("0 9 * * 1").is_ok());
        assert!(Trigger::cron("*/5 * * * *").is_ok());
        assert!(Trigger::cron("not a cron").is_err());
    }

    #[test]
    fn at_trigger_accepts_future_rejects_past() {
        let future = (chrono::Local::now() + chrono::Duration::hours(1)).to_rfc3339();
        assert!(Trigger::at(&future).is_ok());

        let past = (chrono::Local::now() - chrono::Duration::hours(1)).to_rfc3339();
        assert!(Trigger::at(&past).is_err());

        assert!(Trigger::at("not a date").is_err());
        let local_future = (chrono::Local::now() + chrono::Duration::hours(1))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        assert!(Trigger::at(&local_future).is_ok());
    }
}
