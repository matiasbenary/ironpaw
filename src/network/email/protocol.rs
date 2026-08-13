use lettre::{SmtpTransport, Transport, transport::smtp::authentication::Credentials};

use crate::secrets::ToolSecrets;

use super::message::{OutboundEmail, parse_headers, split_message, truncate};
use crate::network::policy::resolve_public_blocking;

fn password(secrets: &ToolSecrets, name: &str) -> Result<String, String> {
    secrets
        .get(name)
        .ok_or_else(|| format!("missing email secret: {name}"))
}

fn imap_session(
    secrets: &ToolSecrets,
    password_secret: &str,
    username: &str,
    host: &str,
    port: u16,
    start_tls: bool,
) -> Result<imap::Session<imap::Connection>, String> {
    resolve_public_blocking(host, port)?;
    let mode = if start_tls {
        imap::ConnectionMode::StartTls
    } else {
        imap::ConnectionMode::Tls
    };
    let client = imap::ClientBuilder::new(host, port)
        .mode(mode)
        .connect()
        .map_err(|err| err.to_string())?;
    client
        .login(username, password(secrets, password_secret)?)
        .map_err(|(err, _)| err.to_string())
}

pub(crate) fn list(
    secrets: &ToolSecrets,
    password_secret: &str,
    username: &str,
    host: &str,
    port: u16,
    start_tls: bool,
    folder: &str,
    limit: u32,
) -> Result<String, String> {
    let mut session = imap_session(secrets, password_secret, username, host, port, start_tls)?;
    session.select(folder).map_err(|err| err.to_string())?;
    let mut uids = session
        .uid_search("ALL")
        .map_err(|err| err.to_string())?
        .into_iter()
        .collect::<Vec<_>>();
    uids.sort_unstable_by(|left, right| right.cmp(left));
    uids.truncate(limit.clamp(1, 50) as usize);
    let uid_set = uids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let messages = if uid_set.is_empty() {
        Vec::new()
    } else {
        session
            .uid_fetch(uid_set, "(UID BODY.PEEK[HEADER])")
            .map_err(|err| err.to_string())?
            .iter()
            .map(|fetch| {
                let headers = parse_headers(fetch.header().unwrap_or_default());
                serde_json::json!({
                    "uid": fetch.uid.map(|uid| uid.to_string()).unwrap_or_default(),
                    "from": headers.get("from").cloned().unwrap_or_default(),
                    "to": headers.get("to").cloned().unwrap_or_default(),
                    "subject": headers.get("subject").cloned().unwrap_or_default(),
                    "date": headers.get("date").cloned().unwrap_or_default(),
                })
            })
            .collect::<Vec<_>>()
    };
    let _ = session.logout();
    Ok(serde_json::json!({ "folder": folder, "messages": messages }).to_string())
}

pub(crate) fn read(
    secrets: &ToolSecrets,
    password_secret: &str,
    username: &str,
    host: &str,
    port: u16,
    start_tls: bool,
    folder: &str,
    uid: u32,
) -> Result<String, String> {
    let mut session = imap_session(secrets, password_secret, username, host, port, start_tls)?;
    session.select(folder).map_err(|err| err.to_string())?;
    let fetches = session
        .uid_fetch(uid.to_string(), "(UID BODY.PEEK[])")
        .map_err(|err| err.to_string())?;
    let fetch = fetches
        .iter()
        .next()
        .ok_or_else(|| format!("email UID {uid} not found in {folder}"))?;
    let (headers, text) = split_message(fetch.body().unwrap_or_default());
    let headers = parse_headers(headers);
    let _ = session.logout();
    Ok(serde_json::json!({
        "folder": folder, "uid": uid.to_string(),
        "from": headers.get("from").cloned().unwrap_or_default(),
        "to": headers.get("to").cloned().unwrap_or_default(),
        "subject": headers.get("subject").cloned().unwrap_or_default(),
        "date": headers.get("date").cloned().unwrap_or_default(),
        "body": truncate(&text, 100_000),
    })
    .to_string())
}

pub(crate) fn send(
    secrets: &ToolSecrets,
    password_secret: &str,
    username: &str,
    from: Option<&str>,
    host: &str,
    port: u16,
    start_tls: bool,
    message_json: &str,
) -> Result<String, String> {
    resolve_public_blocking(host, port)?;
    let email: OutboundEmail = serde_json::from_str(message_json)
        .map_err(|err| format!("invalid email message JSON: {err}"))?;
    if email.to.is_empty() {
        return Err("email must have at least one recipient".to_string());
    }
    let message = email.into_message(
        from.filter(|value| !value.trim().is_empty())
            .unwrap_or(username),
    )?;
    let builder = if start_tls {
        SmtpTransport::starttls_relay(host)
    } else {
        SmtpTransport::relay(host)
    };
    let response = builder
        .map_err(|err| err.to_string())?
        .port(port)
        .credentials(Credentials::new(
            username.to_string(),
            password(secrets, password_secret)?,
        ))
        .build()
        .send(&message)
        .map_err(|err| err.to_string())?;
    Ok(serde_json::json!({
        "ok": true,
        "response": response.message().collect::<Vec<_>>().join(" "),
    })
    .to_string())
}
