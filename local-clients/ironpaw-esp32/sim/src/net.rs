//! Ironpaw's `/client` API over `curl`, with the same calls and bodies as the firmware's `net.rs`.
//! `IRONPAW_URL` (default http://127.0.0.1:7878) and `IRONPAW_CLIENT_TOKEN` (required) say where
//! to connect. No speech recognition: the "audio" of a recording is the text that was typed.

use anyhow::{anyhow, bail, Result};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
    sync::{mpsc::Sender, OnceLock},
    time::Duration,
};

pub use crate::proto::{parse_events, parse_line, Event, Incoming};

const CLIENT_ID: &str = "esp32";
const CONVERSATION_ID: &str = "esp32";

/// `(url, token)`.
static CONFIG: OnceLock<(String, String)> = OnceLock::new();

fn config() -> &'static (String, String) {
    CONFIG.get().expect("net::init runs first")
}

/// Reads the settings from the environment. Call once, before anything else here.
pub fn init() -> Result<()> {
    let token = std::env::var("IRONPAW_CLIENT_TOKEN")
        .map_err(|_| anyhow!("IRONPAW_CLIENT_TOKEN is not set: run `IRONPAW_CLIENT_TOKEN=... cargo +stable run`"))?;
    let url = std::env::var("IRONPAW_URL").unwrap_or_else(|_| "http://127.0.0.1:7878".into());
    let _ = CONFIG.set((url.trim_end_matches('/').to_string(), token));
    Ok(())
}

/// A curl that talks to Ironpaw: quiet, with the token.
fn curl(url: &str) -> Command {
    let mut curl = Command::new("curl");
    curl.args(["-sS", "-H", &format!("Authorization: Bearer {}", config().1), url]);
    curl
}

/// One JSON call. Like the firmware, anything but 2xx is an error reading `<status>: <body>`.
fn call(method: &str, url: &str, body: Option<String>) -> Result<serde_json::Value> {
    let mut curl = curl(url);
    curl.args(["-X", method, "-H", "Content-Type: application/json", "--max-time", "60"]);
    curl.args(["-w", "\n%{http_code}", "--data-binary", "@-"]);
    let mut child = curl.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    child.stdin.take().expect("piped").write_all(body.unwrap_or_default().as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let out = String::from_utf8_lossy(&out.stdout);
    let (body, status) = out.rsplit_once('\n').unwrap_or(("", &out));
    let status: u16 = status.trim().parse()?;
    if !(200..300).contains(&status) {
        bail!("{status}: {body}");
    }
    Ok(serde_json::from_str(body)?)
}

/// Small JSON call on this board's conversation. With `content`, it's sent as the body.
fn conversation(method: &str, path: &str, content: Option<&str>) -> Result<serde_json::Value> {
    let url = format!("{}/client/conversations/{CONVERSATION_ID}/{path}?client_id={CLIENT_ID}", config().0);
    let body = content.map(|content| serde_json::json!({ "client_id": CLIENT_ID, "content": content }).to_string());
    call(method, &url, body)
}

/// The typed text: the recording is that text as bytes, then silence.
fn typed(pcm: &[u8]) -> Result<String> {
    let text = String::from_utf8_lossy(pcm).trim_matches('\0').trim().to_string();
    match text.is_empty() {
        true => bail!("no speech"),
        false => Ok(text),
    }
}

pub fn transcribe(_header: &[u8], pcm: &[u8]) -> Result<String> {
    typed(pcm)
}

pub fn list_events() -> Result<(Vec<Event>, String)> {
    let value = conversation("GET", "events", None)?;
    if let (Some(now), Some(offset)) = (value["now"].as_i64(), value["offset"].as_i64()) {
        crate::nvs::set_time(now, offset as i32);
    }
    Ok((parse_events(&value), value.to_string()))
}

pub fn send_message(text: &str) -> Result<()> {
    conversation("POST", "messages", Some(text)).map(drop)
}

pub fn list_notes() -> Result<Vec<String>> {
    Ok(conversation("GET", "notes", None)?["notes"]
        .as_array()
        .map(|notes| notes.iter().filter_map(|n| n.as_str().map(str::to_string)).collect())
        .unwrap_or_default())
}

/// Thread that keeps the event stream open and retries after 5 s if it drops.
pub fn spawn_events(tx: Sender<Incoming>) -> Result<()> {
    std::thread::spawn(move || loop {
        if let Err(error) = listen(&tx) {
            log::warn!("sse: {error}");
            let _ = tx.send(Incoming::Disconnected(error.to_string()));
        }
        std::thread::sleep(Duration::from_secs(5));
    });
    Ok(())
}

/// One run of the stream. `curl -i` puts the status line first, so a rejected token is reported
/// as `events: HTTP 401`, like the firmware.
fn listen(tx: &Sender<Incoming>) -> Result<()> {
    let url = format!("{}/client/events?client_id={CLIENT_ID}", config().0);
    let mut child = curl(&url).args(["-N", "-i", "-H", "Accept: text/event-stream"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let mut lines = BufReader::new(child.stdout.take().expect("piped")).split(b'\n');
    let status = lines.next().transpose()?.unwrap_or_default();
    let status = String::from_utf8_lossy(&status);
    if status.split(' ').nth(1) != Some("200") {
        let mut stderr = String::new();
        let _ = std::io::Read::read_to_string(&mut child.stderr.take().expect("piped"), &mut stderr);
        let _ = child.wait();
        bail!("{}", match status.split(' ').nth(1) {
            Some(code) => format!("events: HTTP {code}"),
            None => stderr.trim().to_string(),
        });
    }
    // The headers end at the first empty line.
    while let Some(line) = lines.next().transpose()? {
        if line.trim_ascii().is_empty() {
            break;
        }
    }
    let _ = tx.send(Incoming::Connected);
    for line in lines {
        if let Some(incoming) = parse_line(line?.trim_ascii_end()) {
            let _ = tx.send(incoming);
        }
    }
    let _ = child.wait();
    bail!("events: connection closed")
}
