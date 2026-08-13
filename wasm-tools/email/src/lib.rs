wit_bindgen::generate!({
    world: "tool",
    path: "wit/tool.wit",
});

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

mod gmail;
mod outlook;

struct EmailTool;

impl Guest for EmailTool {
    fn name() -> String {
        "email".to_string()
    }

    fn description() -> String {
        "Read Gmail through OAuth or use configured IMAP and SMTP servers. Supports listing recent messages, reading one message, and sending when control access is enabled.".to_string()
    }

    fn parameters() -> String {
        serde_json::to_string(&schemars::schema_for!(EmailParams))
            .unwrap_or_else(|_| "{\"type\":\"object\"}".to_string())
    }

    fn execute(args_json: String, config_json: String) -> Result<String, String> {
        execute_inner(&args_json, &config_json)
    }
}

export!(EmailTool);

#[derive(Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
enum EmailParams {
    /// List recent messages in a folder. Does not mark messages as read.
    List {
        /// Mail folder to inspect. Defaults to INBOX.
        folder: Option<String>,
        /// Maximum number of recent messages to return. Defaults to 10, max 50.
        limit: Option<u32>,
    },
    /// Read one message by IMAP UID. Does not intentionally mark the message as read.
    Read {
        /// Mail folder containing the message. Defaults to INBOX.
        folder: Option<String>,
        /// Stable IMAP UID returned by list.
        uid: String,
    },
    /// Change a Gmail message. Requires Take control access.
    Modify {
        /// Gmail message ID returned by list.
        uid: String,
        /// One of mark_read, mark_unread, archive, or trash.
        operation: EmailOperation,
    },
    /// Send a plain-text email through the configured SMTP server.
    Send {
        /// Recipient email addresses.
        to: Vec<String>,
        /// Email subject.
        subject: String,
        /// Plain-text message body.
        body: String,
        /// Optional CC recipients.
        cc: Option<Vec<String>>,
        /// Optional BCC recipients.
        bcc: Option<Vec<String>>,
        /// Optional From address. Defaults to installed config.
        from: Option<String>,
    },
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum EmailOperation {
    MarkRead,
    MarkUnread,
    Archive,
    Trash,
}

impl EmailOperation {
    fn as_str(&self) -> &'static str {
        match self {
            Self::MarkRead => "mark_read",
            Self::MarkUnread => "mark_unread",
            Self::Archive => "archive",
            Self::Trash => "trash",
        }
    }
}

#[derive(Deserialize, Serialize)]
struct OutboundEmail {
    pub(crate) to: Vec<String>,
    pub(crate) subject: String,
    pub(crate) body: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) cc: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) bcc: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) from: Option<String>,
}

#[derive(Deserialize)]
struct ToolConfig {
    email: EmailConfig,
}

#[derive(Deserialize)]
struct EmailConfig {
    provider: String,
    username: String,
    from: Option<String>,
    oauth: Option<OAuthConfig>,
}

#[derive(Deserialize)]
struct OAuthConfig {
    access: String,
}

fn execute_inner(args_json: &str, config_json: &str) -> Result<String, String> {
    let params: EmailParams =
        serde_json::from_str(args_json).map_err(|err| format!("invalid parameters: {err}"))?;
    let config: ToolConfig =
        serde_json::from_str(config_json).map_err(|err| format!("invalid email config: {err}"))?;
    let access = config
        .email
        .oauth
        .as_ref()
        .map(|oauth| oauth.access.as_str())
        .unwrap_or_default();
    match params {
        EmailParams::List { folder, limit } => {
            let folder = folder_or_inbox(folder);
            let limit = limit.unwrap_or(10).clamp(1, 50);
            match config.email.provider.as_str() {
                "gmail" => gmail::GmailClient::new(
                    &config.email.username,
                    config.email.from.as_deref(),
                    access,
                )
                .list(&folder, limit),
                "outlook" => outlook::OutlookClient::new(access).list(&folder, limit),
                _ => ironpaw::tool::tool_host::imap_list(&folder, limit),
            }
            .map_err(|err| format!("email list failed: {err}"))
        }
        EmailParams::Read { folder, uid } => {
            if uid.trim().is_empty() {
                return Err("'uid' must not be empty".to_string());
            }
            let folder = folder_or_inbox(folder);
            match config.email.provider.as_str() {
                "gmail" => gmail::GmailClient::new(
                    &config.email.username,
                    config.email.from.as_deref(),
                    access,
                )
                .read(&folder, &uid),
                "outlook" => outlook::OutlookClient::new(access).read(&folder, &uid),
                _ => ironpaw::tool::tool_host::imap_read(&folder, &uid),
            }
            .map_err(|err| format!("email read failed: {err}"))
        }
        EmailParams::Modify { uid, operation } => {
            if uid.trim().is_empty() {
                return Err("'uid' must not be empty".to_string());
            }
            match config.email.provider.as_str() {
                "gmail" => gmail::GmailClient::new(
                    &config.email.username,
                    config.email.from.as_deref(),
                    access,
                )
                .modify(&uid, operation.as_str()),
                "outlook" => outlook::OutlookClient::new(access).modify(&uid, operation.as_str()),
                _ => Err("mailbox modifications are unavailable for IMAP email".to_string()),
            }
            .map_err(|err| format!("email modification failed: {err}"))
        }
        EmailParams::Send {
            to,
            subject,
            body,
            cc,
            bcc,
            from,
        } => {
            if to.is_empty() {
                return Err("'to' must contain at least one recipient".to_string());
            }
            if subject.trim().is_empty() {
                return Err("'subject' must not be empty".to_string());
            }
            let outbound = OutboundEmail {
                to,
                subject,
                body,
                cc: cc.unwrap_or_default(),
                bcc: bcc.unwrap_or_default(),
                from,
            };
            let message_json = serde_json::to_string(&outbound).map_err(|err| err.to_string())?;
            match config.email.provider.as_str() {
                "gmail" => gmail::GmailClient::new(
                    &config.email.username,
                    config.email.from.as_deref(),
                    access,
                )
                .send(&message_json),
                "outlook" => outlook::OutlookClient::new(access).send(&message_json),
                _ => ironpaw::tool::tool_host::smtp_send(&message_json),
            }
            .map_err(|err| format!("email send failed: {err}"))
        }
    }
}

fn request(
    method: &str,
    url: &str,
    headers: serde_json::Value,
    body: Option<serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let output = ironpaw::tool::tool_host::http_request_headers(
        method,
        url,
        &headers.to_string(),
        &body.map_or_else(String::new, |value| value.to_string()),
    )?;
    if output.trim().is_empty() {
        Ok(serde_json::Value::Null)
    } else {
        serde_json::from_str(&output).map_err(|err| format!("invalid provider response: {err}"))
    }
}

fn truncate(value: &str, max: usize) -> &str {
    let mut end = value.len().min(max);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

#[cfg(test)]
mod tests {
    use super::execute_inner;

    #[test]
    fn installed_config_is_accepted() {
        let error = execute_inner(
            r#"{"action":"modify","uid":"","operation":"archive"}"#,
            include_str!("../email.config.template.json"),
        )
        .unwrap_err();
        assert_eq!(error, "'uid' must not be empty");
    }

    #[test]
    fn providers_request_least_privilege_read_scopes() {
        let manifest: serde_json::Value =
            serde_json::from_str(include_str!("../email.install.json")).unwrap();
        let configs = manifest["config"].as_array().unwrap();
        let scopes = |provider: &str| {
            configs
                .iter()
                .find(|config| {
                    config["path"] == "/email/oauth/access" && config["when"]["equals"] == provider
                })
                .and_then(|config| config["choices"].as_array())
                .and_then(|choices| choices.iter().find(|choice| choice["value"] == "readonly"))
                .and_then(|choice| choice["oauth"]["scopes"].as_array())
                .cloned()
                .unwrap()
        };
        assert_eq!(
            scopes("gmail"),
            ["https://www.googleapis.com/auth/gmail.readonly"]
        );
        assert_eq!(
            scopes("outlook"),
            ["offline_access", "https://graph.microsoft.com/Mail.Read"]
        );
    }
}

fn folder_or_inbox(folder: Option<String>) -> String {
    let folder = folder.unwrap_or_else(|| "INBOX".to_string());
    if folder.trim().is_empty() {
        "INBOX".to_string()
    } else {
        folder
    }
}
