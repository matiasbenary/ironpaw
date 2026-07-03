use std::str::FromStr;

use crate::bus::{Event, Sender};

#[derive(Clone)]
pub enum Trigger {
    /// Recurring, standard cron expression, evaluated in local time.
    Cron {
        expression: String,
        schedule: Box<cron::Schedule>,
    },
    /// One-shot, fires once at the given instant.
    At {
        datetime: chrono::DateTime<chrono::FixedOffset>,
    },
    /// Fires on inbound user/channel messages. `None` fields match anything.
    MessageIn {
        sender: Option<Sender>,
        channel: Option<String>,
    },
    /// Fires when a conversation-owned tool request has finished.
    ToolResult,
}

impl Trigger {
    /// Accepts the common 5-field cron form (min hour dom mon dow) as well
    /// as the 6/7-field form with seconds (and optional year).
    pub fn cron(expression: &str) -> Result<Self, String> {
        let expression = match expression.split_whitespace().count() {
            5 => format!("0 {expression}"),
            _ => expression.to_string(),
        };
        let schedule = cron::Schedule::from_str(&expression)
            .map_err(|err| format!("invalid cron expression: {err}"))?;
        Ok(Trigger::Cron {
            expression,
            schedule: Box::new(schedule),
        })
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
                Trigger::MessageIn { sender, channel },
                Event::MessageIn {
                    sender: event_sender,
                    channel: event_channel,
                    ..
                },
            ) => {
                sender.as_ref().is_none_or(|sender| sender == event_sender)
                    && channel
                        .as_ref()
                        .is_none_or(|channel| channel == event_channel)
            }
            (
                Trigger::ToolResult,
                Event::ToolResult {
                    origin: crate::bus::ToolOrigin::Conversation { .. },
                    ..
                },
            ) => true,
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
        bus::{ConversationKey, Event},
        message::Message,
    };

    fn message_in(channel: &str, sender: Sender) -> Event {
        Event::MessageIn {
            channel: channel.to_string(),
            sender,
            channel_user_id: None,
            conversation: ConversationKey {
                channel: channel.to_string(),
                chat_id: "c1".to_string(),
                thread_id: None,
            },
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
            sender: Some(Sender::User),
            channel: None,
        };

        assert!(trigger.matches(&message_in("tui", Sender::User)));
        assert!(trigger.matches(&message_in("telegram", Sender::User)));
        assert!(!trigger.matches(&message_in("telegram", Sender::Guest)));
    }

    #[test]
    fn channel_filter_restricts_channel() {
        let trigger = Trigger::MessageIn {
            sender: None,
            channel: Some("telegram".to_string()),
        };
        assert!(trigger.matches(&message_in("telegram", Sender::Guest)));
        assert!(!trigger.matches(&message_in("tui", Sender::User)));
    }

    #[test]
    fn cron_trigger_parses_valid_expressions() {
        assert!(Trigger::cron("0 0 9 * * * *").is_ok());
        assert!(Trigger::cron("not a cron").is_err());
    }

    #[test]
    fn cron_trigger_accepts_standard_five_field_form() {
        assert!(Trigger::cron("0 9 * * 1").is_ok());
        assert!(Trigger::cron("*/5 * * * *").is_ok());
    }

    #[test]
    fn at_trigger_accepts_future_rejects_past() {
        let future = (chrono::Local::now() + chrono::Duration::hours(1)).to_rfc3339();
        assert!(Trigger::at(&future).is_ok());

        let past = (chrono::Local::now() - chrono::Duration::hours(1)).to_rfc3339();
        assert!(Trigger::at(&past).is_err());

        assert!(Trigger::at("not a date").is_err());
    }

    #[test]
    fn at_trigger_accepts_offsetless_local_datetime() {
        let future = (chrono::Local::now() + chrono::Duration::hours(1))
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        assert!(Trigger::at(&future).is_ok());
    }
}
