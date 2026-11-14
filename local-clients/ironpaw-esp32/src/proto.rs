//! What Ironpaw's `/client` API says, as the board understands it: the events and the SSE lines.
//! No ESP-IDF, so the host simulator includes this file too.

/// What arrives from Ironpaw over SSE.
pub enum Incoming {
    Connected,
    Disconnected(String),
    /// The agent is working (writing, calling tools).
    Thinking,
    Reply(String),
    /// A question with options (an approval, a choice): `(label, value)` pairs.
    /// Answering means sending the value as a message.
    Choice(String, Vec<(String, String)>),
    /// The agent failed: an `error` event, or any event carrying an `error` text.
    Failed(String),
}

/// A reminder, as the board keeps it: `at` in local seconds (see `clock`).
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub at: i64,
    pub text: String,
}

pub fn parse_events(value: &serde_json::Value) -> Vec<Event> {
    value["events"]
        .as_array()
        .map(|events| {
            events
                .iter()
                .filter_map(|e| {
                    Some(Event {
                        at: crate::clock::parse(e["at"].as_str()?)?,
                        text: e["text"].as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// One line of the SSE stream: a `data:` line holds an event, anything else is nothing.
pub fn parse_line(line: &[u8]) -> Option<Incoming> {
    parse_event(line.strip_prefix(b"data:")?)
}

fn parse_event(data: &[u8]) -> Option<Incoming> {
    let event: serde_json::Value = serde_json::from_slice(data).ok()?;
    let kind = event["kind"].as_str()?;
    // Ironpaw sends no error events yet; accept any event with an `error` text so a failed turn
    // doesn't leave the board thinking forever.
    if kind == "error" || event["error"].is_string() {
        let text = event["error"].as_str().or(event["message"].as_str()).unwrap_or(kind);
        return Some(Incoming::Failed(text.to_string()));
    }
    match kind {
        // Messages without text are tool-call steps: the agent is still working.
        "message" => match event["content"].as_str()?.trim() {
            "" => Some(Incoming::Thinking),
            text => Some(Incoming::Reply(text.to_string())),
        },
        "activity" => Some(Incoming::Thinking),
        // A question with options: they're shown and answered by voice.
        "choice" => {
            let options = event["options"]
                .as_array()?
                .iter()
                .filter_map(|o| Some((o["label"].as_str()?.to_string(), o["value"].as_str()?.to_string())))
                .collect();
            Some(Incoming::Choice(event["question"].as_str()?.to_string(), options))
        }
        _ => None,
    }
}
