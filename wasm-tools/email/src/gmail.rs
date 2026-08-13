use std::collections::BTreeMap;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

use crate::{OutboundEmail, request, truncate};

const API: &str = "https://gmail.googleapis.com/gmail/v1/users/me";

pub(crate) struct GmailClient<'a> {
    username: &'a str,
    from: Option<&'a str>,
    control: bool,
}

impl<'a> GmailClient<'a> {
    pub(crate) fn new(username: &'a str, from: Option<&'a str>, access: &str) -> Self {
        Self {
            username,
            from,
            control: access == "control",
        }
    }

    pub(crate) fn list(&self, folder: &str, limit: u32) -> Result<String, String> {
        let response = request(
            "GET",
            &format!(
                "{API}/messages?maxResults={}&labelIds={}",
                limit.clamp(1, 50),
                urlencoding::encode(gmail_label(folder))
            ),
            serde_json::json!({}),
            None,
        )?;
        let mut messages = Vec::new();
        for item in response["messages"].as_array().into_iter().flatten() {
            let Some(id) = item["id"].as_str() else {
                continue;
            };
            let message = request(
                "GET",
                &format!(
                    "{API}/messages/{}?format=metadata&metadataHeaders=From&metadataHeaders=To&metadataHeaders=Subject&metadataHeaders=Date",
                    urlencoding::encode(id)
                ),
                serde_json::json!({}),
                None,
            )?;
            let headers = gmail_headers(&message);
            messages.push(serde_json::json!({
                "uid": id,
                "from": headers.get("from").cloned().unwrap_or_default(),
                "to": headers.get("to").cloned().unwrap_or_default(),
                "subject": headers.get("subject").cloned().unwrap_or_default(),
                "date": headers.get("date").cloned().unwrap_or_default(),
            }));
        }
        Ok(serde_json::json!({ "folder": folder, "messages": messages }).to_string())
    }

    pub(crate) fn read(&self, folder: &str, id: &str) -> Result<String, String> {
        let message = request(
            "GET",
            &format!("{API}/messages/{}?format=raw", urlencoding::encode(id)),
            serde_json::json!({}),
            None,
        )?;
        let raw = message["raw"]
            .as_str()
            .ok_or_else(|| "Gmail returned no message body".to_string())?;
        let bytes = URL_SAFE_NO_PAD
            .decode(raw)
            .map_err(|err| format!("invalid Gmail message: {err}"))?;
        let (header_bytes, body) = split_message(&bytes);
        let headers = parse_headers(header_bytes);
        Ok(serde_json::json!({
            "folder": folder, "uid": id,
            "from": headers.get("from").cloned().unwrap_or_default(),
            "to": headers.get("to").cloned().unwrap_or_default(),
            "subject": headers.get("subject").cloned().unwrap_or_default(),
            "date": headers.get("date").cloned().unwrap_or_default(),
            "body": truncate(&body, 100_000),
        })
        .to_string())
    }

    pub(crate) fn send(&self, message_json: &str) -> Result<String, String> {
        self.require_control("send")?;
        let email: OutboundEmail = serde_json::from_str(message_json)
            .map_err(|err| format!("invalid email message JSON: {err}"))?;
        let raw = format_email(
            email,
            self.from
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(self.username),
        )?;
        let response = request(
            "POST",
            &format!("{API}/messages/send"),
            serde_json::json!({}),
            Some(serde_json::json!({ "raw": URL_SAFE_NO_PAD.encode(raw) })),
        )?;
        Ok(serde_json::json!({ "ok": true, "id": response["id"] }).to_string())
    }

    pub(crate) fn modify(&self, id: &str, operation: &str) -> Result<String, String> {
        self.require_control("modify mail")?;
        let id = urlencoding::encode(id);
        let (path, body) = match operation {
            "mark_read" => (
                "modify",
                serde_json::json!({ "removeLabelIds": ["UNREAD"] }),
            ),
            "mark_unread" => ("modify", serde_json::json!({ "addLabelIds": ["UNREAD"] })),
            "archive" => ("modify", serde_json::json!({ "removeLabelIds": ["INBOX"] })),
            "trash" => ("trash", serde_json::json!({})),
            _ => return Err(format!("unsupported Gmail operation: {operation}")),
        };
        let response = request(
            "POST",
            &format!("{API}/messages/{id}/{path}"),
            serde_json::json!({}),
            Some(body),
        )?;
        Ok(serde_json::json!({ "ok": true, "id": response["id"] }).to_string())
    }

    fn require_control(&self, action: &str) -> Result<(), String> {
        self.control.then_some(()).ok_or_else(|| {
            format!("Gmail is connected read-only; reconnect with Take control to {action}")
        })
    }
}

fn gmail_label(folder: &str) -> &str {
    match folder.to_ascii_uppercase().as_str() {
        "INBOX" => "INBOX",
        "SENT" => "SENT",
        "DRAFT" | "DRAFTS" => "DRAFT",
        "TRASH" => "TRASH",
        "SPAM" => "SPAM",
        _ => folder,
    }
}

fn gmail_headers(message: &serde_json::Value) -> BTreeMap<String, String> {
    message["payload"]["headers"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|header| {
            Some((
                header["name"].as_str()?.to_ascii_lowercase(),
                header["value"].as_str()?.to_string(),
            ))
        })
        .collect()
}

fn split_message(message: &[u8]) -> (&[u8], String) {
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

fn parse_headers(bytes: &[u8]) -> BTreeMap<String, String> {
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

fn format_email(email: OutboundEmail, fallback_from: &str) -> Result<String, String> {
    let from = email
        .from
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(fallback_from);
    let headers = [
        ("From", from.to_string()),
        ("To", email.to.join(", ")),
        ("Cc", email.cc.join(", ")),
        ("Bcc", email.bcc.join(", ")),
        ("Subject", email.subject),
    ];
    if headers
        .iter()
        .any(|(_, value)| value.contains(['\r', '\n']))
    {
        return Err("email headers may not contain newlines".to_string());
    }
    let mut message = headers
        .into_iter()
        .filter(|(_, value)| !value.is_empty())
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect::<String>();
    message.push_str("MIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n");
    message.push_str(&email.body);
    Ok(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readonly_client_refuses_to_send_before_network_access() {
        let client = GmailClient::new("me@gmail.com", None, "readonly");
        assert!(
            client
                .send(r#"{"to":["you@example.com"],"subject":"Hi","body":"Hello"}"#)
                .unwrap_err()
                .contains("read-only")
        );
        assert!(
            client
                .modify("message-id", "archive")
                .unwrap_err()
                .contains("read-only")
        );
    }
}
