//! WiFi and Ironpaw's `/client` API: the same protocol the WebUI and TUI use.
//! Voice: `POST /client/transcribe` with the audio as a WAV attachment returns the text.
//! Out: `POST /client/conversations/{id}/messages` with that text.
//! In: `GET /client/events` (SSE) with the agent's replies.
//! Notes: `POST`/`GET`/`DELETE /client/conversations/{id}/notes`, kept by Ironpaw without the agent.
//! Events: `GET /client/conversations/{id}/events` returns the upcoming reminders and the time.

use anyhow::{anyhow, bail, Result};
use base64::Engine as _;
use embedded_svc::http::{client::Client, Method};
use embedded_svc::io::Write;
use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    hal::{delay::FreeRtos, modem::Modem},
    http::client::{Configuration, EspHttpConnection},
    nvs::EspDefaultNvsPartition,
    wifi::{BlockingWifi, ClientConfiguration, Configuration as WifiConfiguration, EspWifi},
};
use std::{
    sync::{mpsc::Sender, OnceLock},
    time::Duration,
};

pub use crate::proto::{parse_events, parse_line, Event, Incoming};

pub const CLIENT_ID: &str = "esp32";
const CONVERSATION_ID: &str = "esp32";

/// Where to connect. Read from NVS (written by `provision.sh`); the build-time variables
/// are only a fallback, so a provisioned board carries no secrets in its firmware image.
pub struct Config {
    pub wifi_ssid: String,
    pub wifi_pass: String,
    pub url: String,
    pub token: String,
}

static CONFIG: OnceLock<Config> = OnceLock::new();

pub fn config() -> &'static Config {
    CONFIG.get().expect("net::init runs first")
}

/// Loads the settings from NVS. Call once, before anything else here.
pub fn init(partition: EspDefaultNvsPartition) -> Result<()> {
    crate::nvs::open(partition)?;
    let get = |key: &str, fallback: Option<&str>| -> Result<String> {
        match crate::nvs::get_str(key)? {
            Some(value) => Ok(value),
            None => fallback
                .map(str::to_string)
                .ok_or_else(|| anyhow!("{key} is not set: run provision.sh")),
        }
    };
    let config = Config {
        wifi_ssid: get("wifi_ssid", option_env!("WIFI_SSID"))?,
        wifi_pass: get("wifi_pass", option_env!("WIFI_PASS"))?,
        url: get("url", option_env!("IRONPAW_URL"))?,
        token: get("token", option_env!("IRONPAW_TOKEN"))?,
    };
    let _ = CONFIG.set(config);
    Ok(())
}

pub fn connect_wifi(modem: Modem, partition: EspDefaultNvsPartition) -> Result<BlockingWifi<EspWifi<'static>>> {
    let sysloop = EspSystemEventLoop::take()?;
    let mut wifi = BlockingWifi::wrap(EspWifi::new(modem, sysloop.clone(), Some(partition))?, sysloop)?;
    wifi.set_configuration(&WifiConfiguration::Client(ClientConfiguration {
        ssid: config().wifi_ssid.as_str().try_into().map_err(|_| anyhow!("WiFi name too long"))?,
        password: config().wifi_pass.as_str().try_into().map_err(|_| anyhow!("WiFi password too long"))?,
        ..Default::default()
    }))?;
    wifi.start()?;
    reconnect(&mut wifi)?;
    Ok(wifi)
}

pub fn reconnect(wifi: &mut BlockingWifi<EspWifi<'static>>) -> Result<()> {
    wifi.connect()?;
    wifi.wait_netif_up()?;
    Ok(())
}

pub fn client(timeout: Duration) -> Result<Client<EspHttpConnection>> {
    Ok(Client::wrap(EspHttpConnection::new(&Configuration {
        timeout: Some(timeout),
        // HTTPS works too (Ironpaw behind a TLS proxy), with the usual public CAs.
        crt_bundle_attach: Some(esp_idf_svc::sys::esp_crt_bundle_attach),
        ..Default::default()
    })?))
}

/// Sends a WAV and returns what Ironpaw transcribed. Nothing reaches the agent yet:
/// the board decides whether it's a message, a note or a command.
pub fn transcribe(header: &[u8], pcm: &[u8]) -> Result<String> {
    let mut wav = Vec::with_capacity(header.len() + pcm.len());
    wav.extend_from_slice(header);
    wav.extend_from_slice(pcm);
    let value = post_wav(&format!("{}/client/transcribe", config().url), wav)?;
    Ok(value["text"].as_str().unwrap_or_default().to_string())
}

/// POSTs a WAV as the single attachment of a client message and returns the JSON reply.
fn post_wav(url: &str, wav: Vec<u8>) -> Result<serde_json::Value> {
    let audio = base64::engine::general_purpose::STANDARD.encode(&wav);
    drop(wav);
    // The JSON is built by hand and sent in three parts to avoid copying the base64 (~430 KB).
    let prefix = format!(
        r#"{{"client_id":"{CLIENT_ID}","attachments":[{{"filename":"voice.wav","mime_type":"audio/wav","data_base64":""#
    );
    let suffix = r#""}]}"#;
    let len = (prefix.len() + audio.len() + suffix.len()).to_string();
    let auth = format!("Bearer {}", config().token);
    let headers = [
        ("authorization", auth.as_str()),
        ("content-type", "application/json"),
        ("content-length", len.as_str()),
    ];
    // Transcription takes a few seconds: long timeout.
    let mut client = client(Duration::from_secs(60))?;
    let mut request = client.post(url, &headers)?;
    request.write_all(prefix.as_bytes())?;
    request.write_all(audio.as_bytes())?;
    request.write_all(suffix.as_bytes())?;
    request.flush()?;
    let mut response = request.submit()?;
    let status = response.status();
    read_json(status, &mut response)
}

/// The upcoming reminders from Ironpaw. Also sets the clock from the reply and returns the
/// raw JSON, which the caller caches on the SD card for when Ironpaw is unreachable.
pub fn list_events() -> Result<(Vec<Event>, String)> {
    let value = call(Method::Get, "events", None)?;
    if let (Some(now), Some(offset)) = (value["now"].as_i64(), value["offset"].as_i64()) {
        crate::nvs::set_time(now, offset as i32);
    }
    Ok((parse_events(&value), value.to_string()))
}

/// Sends the transcript to the agent, like a typed message. The reply arrives over SSE.
pub fn send_message(text: &str) -> Result<()> {
    call(Method::Post, "messages", Some(text)).map(drop)
}

/// The latest notes, newest first.
pub fn list_notes() -> Result<Vec<String>> {
    Ok(call(Method::Get, "notes", None)?["notes"]
        .as_array()
        .map(|notes| notes.iter().filter_map(|n| n.as_str().map(str::to_string)).collect())
        .unwrap_or_default())
}

/// Small JSON call on this board's conversation. With `content`, it's sent as the body.
fn call(method: Method, path: &str, content: Option<&str>) -> Result<serde_json::Value> {
    let auth = format!("Bearer {}", config().token);
    let url = format!("{}/client/conversations/{CONVERSATION_ID}/{path}?client_id={CLIENT_ID}", config().url);
    let body = content
        .map(|content| serde_json::json!({ "client_id": CLIENT_ID, "content": content }).to_string())
        .unwrap_or_default();
    let len = body.len().to_string();
    let headers = [
        ("authorization", auth.as_str()),
        ("content-type", "application/json"),
        ("content-length", len.as_str()),
    ];
    let mut client = client(Duration::from_secs(15))?;
    let mut request = client.request(method, &url, &headers)?;
    request.write_all(body.as_bytes())?;
    request.flush()?;
    let mut response = request.submit()?;
    let status = response.status();
    read_json(status, &mut response)
}

fn read_json(status: u16, response: &mut impl embedded_svc::io::Read) -> Result<serde_json::Value> {
    let mut body = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        let n = response.read(&mut buf).map_err(|e| anyhow!("{e:?}"))?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buf[..n]);
    }
    let body = String::from_utf8_lossy(&body);
    if !(200..300).contains(&status) {
        bail!("{status}: {body}");
    }
    Ok(serde_json::from_str(&body)?)
}

/// Asks Ironpaw for the spoken reply (Piper on the PC) and feeds it to `play` in blocks
/// of mono s16le PCM at 16 kHz as it arrives. `play` returns `false` to stop.
pub fn speech(text: &str, mut play: impl FnMut(&[u8]) -> Result<bool>) -> Result<()> {
    let body = serde_json::json!({ "text": text }).to_string();
    let len = body.len().to_string();
    let auth = format!("Bearer {}", config().token);
    let url = format!("{}/client/speech", config().url);
    let headers = [
        ("authorization", auth.as_str()),
        ("content-type", "application/json"),
        ("content-length", len.as_str()),
    ];
    // Ironpaw streams Piper's audio sentence by sentence; the timeout is per read.
    let mut client = client(Duration::from_secs(60))?;
    let mut request = client.post(&url, &headers)?;
    request.write_all(body.as_bytes())?;
    request.flush()?;
    let mut response = request.submit()?;
    let status = response.status();
    if !(200..300).contains(&status) {
        bail!("speech: HTTP {status}");
    }
    let mut buf = [0u8; 512];
    loop {
        // Full block (or the tail): this way a sample is never split in half.
        let mut n = 0;
        while n < buf.len() {
            match response.read(&mut buf[n..])? {
                0 => break,
                read => n += read,
            }
        }
        if n == 0 || !play(&buf[..n & !1])? || n < buf.len() {
            return Ok(());
        }
    }
}

/// Thread that keeps the SSE open and reconnects with backoff if it drops.
pub fn spawn_events(tx: Sender<Incoming>) -> Result<()> {
    std::thread::Builder::new().stack_size(12 * 1024).spawn(move || {
        let mut backoff_ms = 1_000;
        loop {
            if let Err(error) = listen(&tx) {
                log::warn!("sse: {error}");
                let _ = tx.send(Incoming::Disconnected(error.to_string()));
            }
            FreeRtos::delay_ms(backoff_ms);
            backoff_ms = (backoff_ms * 2).min(30_000);
        }
    })?;
    Ok(())
}

fn listen(tx: &Sender<Incoming>) -> Result<()> {
    let auth = format!("Bearer {}", config().token);
    let url = format!("{}/client/events?client_id={CLIENT_ID}", config().url);
    // Ironpaw sends a keep-alive every 15 s: 45 s of silence means the connection is dead.
    let mut client = client(Duration::from_secs(45))?;
    let headers = [("authorization", auth.as_str()), ("accept", "text/event-stream")];
    let mut response = client.request(Method::Get, &url, &headers)?.submit()?;
    if response.status() != 200 {
        bail!("events: HTTP {}", response.status());
    }
    let _ = tx.send(Incoming::Connected);
    let mut line = Vec::new();
    let mut byte = [0u8];
    loop {
        // ponytail: one byte at a time, because esp_http_client_read waits to fill the buffer and a
        // small event would get stuck. Good enough for text messages; read in blocks if large
        // attachments ever arrive (TTS audio).
        if response.read(&mut byte)? == 0 {
            bail!("events: connection closed");
        }
        if byte[0] != b'\n' {
            line.push(byte[0]);
            continue;
        }
        if let Some(incoming) = parse_line(&line) {
            let _ = tx.send(incoming);
        }
        line.clear();
    }
}
