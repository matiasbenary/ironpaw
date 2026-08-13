use std::collections::BTreeMap;

use lettre::{Message, message::header::ContentType};

#[derive(serde::Deserialize)]
pub(super) struct OutboundEmail {
    pub to: Vec<String>,
    pub subject: String,
    pub body: String,
    #[serde(default)]
    pub cc: Vec<String>,
    #[serde(default)]
    pub bcc: Vec<String>,
    #[serde(default)]
    pub from: Option<String>,
}

impl OutboundEmail {
    pub fn into_message(self, fallback_from: &str) -> Result<Message, String> {
        let from = self
            .from
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(fallback_from);
        let mut builder = Message::builder()
            .from(
                from.parse()
                    .map_err(|error| format!("invalid from address: {error}"))?,
            )
            .subject(self.subject)
            .header(ContentType::TEXT_PLAIN);
        for address in self.to {
            builder = builder.to(address
                .parse()
                .map_err(|error| format!("invalid to address: {error}"))?);
        }
        for address in self.cc {
            builder = builder.cc(address
                .parse()
                .map_err(|error| format!("invalid cc address: {error}"))?);
        }
        for address in self.bcc {
            builder = builder.bcc(
                address
                    .parse()
                    .map_err(|error| format!("invalid bcc address: {error}"))?,
            );
        }
        builder.body(self.body).map_err(|error| error.to_string())
    }
}

pub(super) fn split_message(message: &[u8]) -> (&[u8], String) {
    for (separator, len) in [(b"\r\n\r\n".as_slice(), 4), (b"\n\n".as_slice(), 2)] {
        if let Some(index) = message.windows(len).position(|window| window == separator) {
            return (
                &message[..index],
                String::from_utf8_lossy(&message[index + len..]).into_owned(),
            );
        }
    }
    (message, String::new())
}

pub(super) fn parse_headers(bytes: &[u8]) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::new();
    let mut current: Option<(String, String)> = None;
    for line in String::from_utf8_lossy(bytes).lines() {
        if line.starts_with([' ', '\t']) {
            if let Some((_, value)) = current.as_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
        } else {
            if let Some((name, value)) = current.take() {
                headers.insert(name, value);
            }
            current = line
                .split_once(':')
                .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    if let Some((name, value)) = current {
        headers.insert(name, value);
    }
    headers
}

pub(super) fn truncate(value: &str, max: usize) -> &str {
    let mut end = value.len().min(max);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}
