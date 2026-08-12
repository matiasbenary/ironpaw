wit_bindgen::generate!({
    world: "tool",
    path: "../../wit/tool.wit",
});

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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

    fn execute(args_json: String, _config_json: String) -> Result<String, String> {
        execute_inner(&args_json)
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

#[derive(Serialize)]
struct OutboundEmail {
    to: Vec<String>,
    subject: String,
    body: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cc: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    bcc: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<String>,
}

fn execute_inner(args_json: &str) -> Result<String, String> {
    let params: EmailParams =
        serde_json::from_str(args_json).map_err(|err| format!("invalid parameters: {err}"))?;
    match params {
        EmailParams::List { folder, limit } => {
            let folder = folder_or_inbox(folder);
            let limit = limit.unwrap_or(10).clamp(1, 50);
            ironpaw::tool::tool_host::email_list(&folder, limit)
                .map_err(|err| format!("email list failed: {err}"))
        }
        EmailParams::Read { folder, uid } => {
            if uid.trim().is_empty() {
                return Err("'uid' must not be empty".to_string());
            }
            let folder = folder_or_inbox(folder);
            ironpaw::tool::tool_host::email_read(&folder, &uid)
                .map_err(|err| format!("email read failed: {err}"))
        }
        EmailParams::Modify { uid, operation } => {
            if uid.trim().is_empty() {
                return Err("'uid' must not be empty".to_string());
            }
            ironpaw::tool::tool_host::email_modify(&uid, operation.as_str())
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
            ironpaw::tool::tool_host::email_send(&message_json)
                .map_err(|err| format!("email send failed: {err}"))
        }
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
