use std::collections::BTreeMap;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::blocking::Client;

use crate::{
    secrets::ToolSecrets,
    tools::email::{OutboundEmail, parse_headers, split_message, truncate},
};

const API: &str = "https://gmail.googleapis.com/gmail/v1/users/me";

pub(super) struct GmailClient<'a> {
    secrets: &'a ToolSecrets,
    refresh_token_secret: &'a str,
    username: &'a str,
    from: Option<&'a str>,
    control: bool,
}

impl<'a> GmailClient<'a> {
    pub(super) fn new(
        secrets: &'a ToolSecrets,
        refresh_token_secret: &'a str,
        username: &'a str,
        from: Option<&'a str>,
        access: &str,
    ) -> Self {
        Self {
            secrets,
            refresh_token_secret,
            username,
            from,
            control: access == "control",
        }
    }

    pub(super) fn list(&self, folder: &str, limit: u32) -> Result<String, String> {
        let client = self.client()?;
        let label = gmail_label(folder);
        let response: serde_json::Value = client
            .get(format!("{API}/messages"))
            .query(&[
                ("maxResults", limit.clamp(1, 50).to_string()),
                ("labelIds", label.to_string()),
            ])
            .send()
            .map_err(http_error)?
            .error_for_status()
            .map_err(http_error)?
            .json()
            .map_err(http_error)?;
        let mut messages = Vec::new();
        for item in response["messages"].as_array().into_iter().flatten() {
            let Some(id) = item["id"].as_str() else {
                continue;
            };
            let message: serde_json::Value = client
                .get(format!("{API}/messages/{id}"))
                .query(&[
                    ("format", "metadata"),
                    ("metadataHeaders", "From"),
                    ("metadataHeaders", "To"),
                    ("metadataHeaders", "Subject"),
                    ("metadataHeaders", "Date"),
                ])
                .send()
                .map_err(http_error)?
                .error_for_status()
                .map_err(http_error)?
                .json()
                .map_err(http_error)?;
            let headers = gmail_headers(&message);
            messages.push(serde_json::json!({
                "uid": id,
                "from": headers.get("from").cloned().unwrap_or_default(),
                "to": headers.get("to").cloned().unwrap_or_default(),
                "subject": headers.get("subject").cloned().unwrap_or_default(),
                "date": headers.get("date").cloned().unwrap_or_default(),
            }));
        }
        serde_json::to_string(&serde_json::json!({ "folder": folder, "messages": messages }))
            .map_err(|e| e.to_string())
    }

    pub(super) fn read(&self, folder: &str, id: &str) -> Result<String, String> {
        let client = self.client()?;
        let message: serde_json::Value = client
            .get(format!("{API}/messages/{id}"))
            .query(&[("format", "raw")])
            .send()
            .map_err(http_error)?
            .error_for_status()
            .map_err(http_error)?
            .json()
            .map_err(http_error)?;
        let raw = message["raw"]
            .as_str()
            .ok_or_else(|| "Gmail returned no message body".to_string())?;
        let bytes = URL_SAFE_NO_PAD
            .decode(raw)
            .map_err(|e| format!("invalid Gmail message: {e}"))?;
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

    pub(super) fn send(&self, message_json: &str) -> Result<String, String> {
        if !self.control {
            return Err(
                "Gmail is connected read-only; reconnect with Take control to send".to_string(),
            );
        }
        let email: OutboundEmail = serde_json::from_str(message_json)
            .map_err(|e| format!("invalid email message JSON: {e}"))?;
        let fallback_from = self
            .from
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(self.username);
        let raw = email.into_message(fallback_from)?.formatted();
        let response: serde_json::Value = self
            .client()?
            .post(format!("{API}/messages/send"))
            .json(&serde_json::json!({ "raw": URL_SAFE_NO_PAD.encode(raw) }))
            .send()
            .map_err(http_error)?
            .error_for_status()
            .map_err(http_error)?
            .json()
            .map_err(http_error)?;
        Ok(serde_json::json!({ "ok": true, "id": response["id"] }).to_string())
    }

    pub(super) fn modify(&self, id: &str, operation: &str) -> Result<String, String> {
        if !self.control {
            return Err(
                "Gmail is connected read-only; reconnect with Take control to modify mail"
                    .to_string(),
            );
        }
        let client = self.client()?;
        let response = match operation {
            "mark_read" => client
                .post(format!("{API}/messages/{id}/modify"))
                .json(&serde_json::json!({ "removeLabelIds": ["UNREAD"] })),
            "mark_unread" => client
                .post(format!("{API}/messages/{id}/modify"))
                .json(&serde_json::json!({ "addLabelIds": ["UNREAD"] })),
            "archive" => client
                .post(format!("{API}/messages/{id}/modify"))
                .json(&serde_json::json!({ "removeLabelIds": ["INBOX"] })),
            "trash" => client.post(format!("{API}/messages/{id}/trash")),
            _ => return Err(format!("unsupported Gmail operation: {operation}")),
        };
        let message: serde_json::Value = response
            .send()
            .map_err(http_error)?
            .error_for_status()
            .map_err(http_error)?
            .json()
            .map_err(http_error)?;
        Ok(serde_json::json!({ "ok": true, "id": message["id"] }).to_string())
    }

    fn client(&self) -> Result<Client, String> {
        let refresh_token = self
            .secrets
            .get(self.refresh_token_secret)
            .ok_or_else(|| format!("missing Gmail secret: {}", self.refresh_token_secret))?;
        crate::oauth::authenticated_client(
            crate::tools::email_host::oauth_settings(
                crate::tools::email_host::Provider::Google,
                if self.control { "control" } else { "readonly" },
            )?,
            &refresh_token,
        )
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
        .filter_map(|h| {
            Some((
                h["name"].as_str()?.to_ascii_lowercase(),
                h["value"].as_str()?.to_string(),
            ))
        })
        .collect()
}
fn http_error(error: reqwest::Error) -> String {
    format!("Gmail request failed: {}", error.without_url())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::Secrets;

    #[test]
    fn readonly_client_refuses_to_send_before_network_access() {
        let secrets = Secrets::for_tests();
        let client = GmailClient::new(
            &secrets.tools,
            "EMAIL_GOOGLE_REFRESH_TOKEN",
            "me@gmail.com",
            None,
            "readonly",
        );

        let error = client
            .send(r#"{"to":["you@example.com"],"subject":"Hi","body":"Hello"}"#)
            .unwrap_err();

        assert!(error.contains("read-only"));
        assert!(
            client
                .modify("message-id", "archive")
                .unwrap_err()
                .contains("read-only")
        );
    }
}
