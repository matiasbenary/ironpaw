use crate::{OutboundEmail, request, truncate};

const API: &str = "https://graph.microsoft.com/v1.0/me";

pub(crate) struct OutlookClient {
    account: String,
    control: bool,
}

impl OutlookClient {
    pub(crate) fn new(account: &str, access: &str) -> Self {
        Self {
            account: account.to_string(),
            control: access == "control",
        }
    }

    pub(crate) fn list(&self, folder: &str, limit: u32) -> Result<String, String> {
        let folder = graph_folder(folder);
        let response = request(
            &self.account,
            "GET",
            &format!(
                "{API}/mailFolders/{}/messages?$top={}&$select=id%2Cfrom%2CtoRecipients%2Csubject%2CreceivedDateTime&$orderby=receivedDateTime%20desc",
                urlencoding::encode(folder),
                limit.clamp(1, 50)
            ),
            serde_json::json!({}),
            None,
        )?;
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

    pub(crate) fn read(&self, folder: &str, id: &str) -> Result<String, String> {
        let message = request(
            &self.account,
            "GET",
            &format!(
                "{API}/messages/{}?$select=id%2Cfrom%2CtoRecipients%2Csubject%2CreceivedDateTime%2Cbody",
                urlencoding::encode(id)
            ),
            serde_json::json!({ "Prefer": "outlook.body-content-type=\"text\"" }),
            None,
        )?;
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

    pub(crate) fn send(&self, message_json: &str) -> Result<String, String> {
        self.require_control("send")?;
        let email: OutboundEmail = serde_json::from_str(message_json)
            .map_err(|err| format!("invalid email message JSON: {err}"))?;
        request(
            &self.account,
            "POST",
            &format!("{API}/sendMail"),
            serde_json::json!({}),
            Some(serde_json::json!({
                "message": {
                    "subject": email.subject,
                    "body": { "contentType": "Text", "content": email.body },
                    "toRecipients": recipients(email.to),
                    "ccRecipients": recipients(email.cc),
                    "bccRecipients": recipients(email.bcc),
                },
                "saveToSentItems": true,
            })),
        )?;
        Ok(serde_json::json!({ "ok": true }).to_string())
    }

    pub(crate) fn modify(&self, id: &str, operation: &str) -> Result<String, String> {
        self.require_control("modify mail")?;
        let id = urlencoding::encode(id);
        let (method, url, body) = match operation {
            "mark_read" => (
                "PATCH",
                format!("{API}/messages/{id}"),
                serde_json::json!({ "isRead": true }),
            ),
            "mark_unread" => (
                "PATCH",
                format!("{API}/messages/{id}"),
                serde_json::json!({ "isRead": false }),
            ),
            "archive" => (
                "POST",
                format!("{API}/messages/{id}/move"),
                serde_json::json!({ "destinationId": "archive" }),
            ),
            "trash" => (
                "POST",
                format!("{API}/messages/{id}/move"),
                serde_json::json!({ "destinationId": "deleteditems" }),
            ),
            _ => return Err(format!("unsupported Outlook operation: {operation}")),
        };
        let message = request(&self.account, method, &url, serde_json::json!({}), Some(body))?;
        Ok(serde_json::json!({ "ok": true, "id": message["id"] }).to_string())
    }

    fn require_control(&self, action: &str) -> Result<(), String> {
        self.control.then_some(()).ok_or_else(|| {
            format!("Outlook is connected read-only; reconnect with Take control to {action}")
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readonly_client_refuses_mutations_without_network_access() {
        let client = OutlookClient::new("me@outlook.com", "readonly");
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
