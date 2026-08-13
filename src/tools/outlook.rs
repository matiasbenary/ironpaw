use reqwest::blocking::Client;

use crate::{
    secrets::ToolSecrets,
    tools::email::{OutboundEmail, truncate},
};

const API: &str = "https://graph.microsoft.com/v1.0/me";

pub(super) struct OutlookClient<'a> {
    secrets: &'a ToolSecrets,
    refresh_token_secret: &'a str,
    control: bool,
}

impl<'a> OutlookClient<'a> {
    pub(super) fn new(
        secrets: &'a ToolSecrets,
        refresh_token_secret: &'a str,
        access: &str,
    ) -> Self {
        Self {
            secrets,
            refresh_token_secret,
            control: access == "control",
        }
    }

    pub(super) fn list(&self, folder: &str, limit: u32) -> Result<String, String> {
        let folder = graph_folder(folder);
        let response: serde_json::Value = self
            .client()?
            .get(format!("{API}/mailFolders/{folder}/messages"))
            .query(&[
                ("$top", limit.clamp(1, 50).to_string()),
                (
                    "$select",
                    "id,from,toRecipients,subject,receivedDateTime".to_string(),
                ),
                ("$orderby", "receivedDateTime desc".to_string()),
            ])
            .send()
            .map_err(http_error)?
            .error_for_status()
            .map_err(http_error)?
            .json()
            .map_err(http_error)?;
        let messages = response["value"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|message| {
                serde_json::json!({
                    "uid": message["id"],
                    "from": address(&message["from"]),
                    "to": addresses(&message["toRecipients"]),
                    "subject": message["subject"].as_str().unwrap_or_default(),
                    "date": message["receivedDateTime"].as_str().unwrap_or_default(),
                })
            })
            .collect::<Vec<_>>();
        Ok(serde_json::json!({ "folder": folder, "messages": messages }).to_string())
    }

    pub(super) fn read(&self, folder: &str, id: &str) -> Result<String, String> {
        let message: serde_json::Value = self
            .client()?
            .get(format!("{API}/messages/{id}"))
            .query(&[(
                "$select",
                "id,from,toRecipients,subject,receivedDateTime,body",
            )])
            .header("Prefer", "outlook.body-content-type=\"text\"")
            .send()
            .map_err(http_error)?
            .error_for_status()
            .map_err(http_error)?
            .json()
            .map_err(http_error)?;
        Ok(serde_json::json!({
            "folder": folder, "uid": id,
            "from": address(&message["from"]),
            "to": addresses(&message["toRecipients"]),
            "subject": message["subject"].as_str().unwrap_or_default(),
            "date": message["receivedDateTime"].as_str().unwrap_or_default(),
            "body": truncate(message["body"]["content"].as_str().unwrap_or_default(), 100_000),
        })
        .to_string())
    }

    pub(super) fn send(&self, message_json: &str) -> Result<String, String> {
        self.require_control("send")?;
        let email: OutboundEmail = serde_json::from_str(message_json)
            .map_err(|error| format!("invalid email message JSON: {error}"))?;
        let message = serde_json::json!({
            "subject": email.subject,
            "body": { "contentType": "Text", "content": email.body },
            "toRecipients": recipients(email.to),
            "ccRecipients": recipients(email.cc),
            "bccRecipients": recipients(email.bcc),
        });
        self.client()?
            .post(format!("{API}/sendMail"))
            .json(&serde_json::json!({ "message": message, "saveToSentItems": true }))
            .send()
            .map_err(http_error)?
            .error_for_status()
            .map_err(http_error)?;
        Ok(serde_json::json!({ "ok": true }).to_string())
    }

    pub(super) fn modify(&self, id: &str, operation: &str) -> Result<String, String> {
        self.require_control("modify mail")?;
        let client = self.client()?;
        let response = match operation {
            "mark_read" => client
                .patch(format!("{API}/messages/{id}"))
                .json(&serde_json::json!({ "isRead": true })),
            "mark_unread" => client
                .patch(format!("{API}/messages/{id}"))
                .json(&serde_json::json!({ "isRead": false })),
            "archive" => client
                .post(format!("{API}/messages/{id}/move"))
                .json(&serde_json::json!({ "destinationId": "archive" })),
            "trash" => client
                .post(format!("{API}/messages/{id}/move"))
                .json(&serde_json::json!({ "destinationId": "deleteditems" })),
            _ => return Err(format!("unsupported Outlook operation: {operation}")),
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
            .ok_or_else(|| format!("missing Outlook secret: {}", self.refresh_token_secret))?;
        crate::oauth::authenticated_client(
            crate::tools::email_host::oauth_settings(
                crate::tools::email_host::Provider::Microsoft,
                if self.control { "control" } else { "readonly" },
            )?,
            &refresh_token,
        )
    }

    fn require_control(&self, action: &str) -> Result<(), String> {
        if self.control {
            Ok(())
        } else {
            Err(format!(
                "Outlook is connected read-only; reconnect with Take control to {action}"
            ))
        }
    }
}

fn graph_folder(folder: &str) -> &str {
    match folder.to_ascii_uppercase().as_str() {
        "INBOX" => "inbox",
        "SENT" => "sentitems",
        "DRAFT" | "DRAFTS" => "drafts",
        "TRASH" => "deleteditems",
        "SPAM" => "junkemail",
        _ => folder,
    }
}
fn address(value: &serde_json::Value) -> String {
    value["emailAddress"]["address"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}
fn addresses(value: &serde_json::Value) -> String {
    value
        .as_array()
        .into_iter()
        .flatten()
        .map(address)
        .collect::<Vec<_>>()
        .join(", ")
}
fn recipients(values: Vec<String>) -> Vec<serde_json::Value> {
    values
        .into_iter()
        .map(|address| serde_json::json!({ "emailAddress": { "address": address } }))
        .collect()
}
fn http_error(error: reqwest::Error) -> String {
    format!("Outlook request failed: {}", error.without_url())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::Secrets;

    #[test]
    fn readonly_client_refuses_mutations_without_network_access() {
        let secrets = Secrets::for_tests();
        let client =
            OutlookClient::new(&secrets.tools, "EMAIL_MICROSOFT_REFRESH_TOKEN", "readonly");
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
