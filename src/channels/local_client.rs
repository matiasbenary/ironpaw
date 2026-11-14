use std::{convert::Infallible, path::Path, path::PathBuf, sync::Arc};

use axum::{
    Json, Router,
    extract::{Path as RoutePath, Query, State},
    http::{HeaderMap, StatusCode},
    middleware,
    response::{Sse, sse::Event as SseEvent},
    routing::{get, post},
};
use futures::{StreamExt as _, stream};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::{
    bus::{ChannelAction, ConversationKey, Event},
    channels::bridge::{Bridge, Channel, Delivery, DeliveryFuture, InboundMessage},
    inference::Inference,
    message::{Attachment, Message, ToolCall},
    network::server::preflight,
};

use super::wasm::attachment_from_content;

pub(crate) const CHANNEL: &str = "local-client";

pub(crate) struct LocalClient {
    bridge: Bridge,
    files_dir: PathBuf,
    token: String,
    /// Optional second token for devices that keep it in flash (the ESP32): it only works
    /// for `DEVICE_CLIENT_ID`, so a leaked one can't read other clients' conversations.
    device_token: Option<String>,
    events: broadcast::Sender<ClientEvent>,
    inference: Inference,
}

impl LocalClient {
    pub(crate) fn new(
        bridge: Bridge,
        files_dir: PathBuf,
        inference: Inference,
        token: String,
        device_token: Option<String>,
    ) -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        Arc::new(Self {
            bridge,
            files_dir,
            token,
            device_token,
            events,
            inference,
        })
    }

    pub(crate) fn connection_token(&self) -> &str {
        &self.token
    }

    pub(crate) fn routes(self: &Arc<Self>) -> Router {
        Router::new()
            .route(
                "/client/conversations/{conversation_id}/messages",
                post(send_message).options(preflight),
            )
            .route("/client/events", get(events).options(preflight))
            .route("/client/status", get(status).options(preflight))
            .route("/client/speech", post(speech).options(preflight))
            .route("/client/transcribe", post(transcribe).options(preflight))
            .route(
                "/client/conversations/{conversation_id}/notes",
                get(list_notes).options(preflight),
            )
            .route(
                "/client/conversations/{conversation_id}/events",
                get(list_events).options(preflight),
            )
            .layer(middleware::from_fn(crate::network::server::cors))
            .with_state(Arc::clone(self))
    }

    fn authorize(
        &self,
        headers: &HeaderMap,
        client_id: Option<&str>,
    ) -> Result<(), (StatusCode, String)> {
        authorize(
            headers,
            &self.token,
            self.device_token.as_deref(),
            client_id,
        )
    }

    fn receive(
        &self,
        client_id: String,
        conversation_id: &str,
        message_id: Option<String>,
        content: String,
        attachments: Vec<IncomingAttachment>,
    ) -> Result<(), String> {
        receive_message(
            &self.bridge,
            &self.files_dir,
            client_id,
            conversation_id,
            message_id,
            content,
            attachments,
        )
    }
}

#[derive(Deserialize)]
struct IncomingAttachment {
    filename: String,
    mime_type: String,
    data_base64: String,
}

fn receive_message(
    bridge: &Bridge,
    files_dir: &Path,
    client_id: String,
    conversation_id: &str,
    message_id: Option<String>,
    content: String,
    incoming_attachments: Vec<IncomingAttachment>,
) -> Result<(), String> {
    let mut text = Vec::new();
    if !content.trim().is_empty() {
        text.push(content);
    }
    let mut attachments = Vec::new();
    for attachment in incoming_attachments {
        match attachment_from_content(
            Some(attachment.filename),
            attachment.mime_type,
            attachment.data_base64,
            files_dir,
        ) {
            Ok(attachment) => attachments.push(attachment),
            Err(error) => text.push(error),
        }
    }

    let conversation = conversation(&client_id, conversation_id);
    let message = Message::User {
        text: text.join("\n\n"),
        attachments,
    };
    bridge.receive(InboundMessage {
        sender: crate::bus::Sender::User(client_id),
        conversation,
        message_id,
        message,
    })
}

#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ClientEvent {
    Message {
        client_id: String,
        conversation_id: String,
        id: String,
        content: String,
        attachments: Vec<ClientAttachment>,
        tool_calls: Vec<ToolCall>,
    },
    Delete {
        client_id: String,
        conversation_id: String,
        message_id: String,
    },
    Activity {
        client_id: String,
        conversation_id: String,
        action: ChannelAction,
    },
    Choice {
        client_id: String,
        conversation_id: String,
        id: String,
        question: String,
        options: Vec<crate::bus::UserInputOption>,
    },
}

impl ClientEvent {
    fn client_id(&self) -> &str {
        match self {
            Self::Message { client_id, .. }
            | Self::Delete { client_id, .. }
            | Self::Activity { client_id, .. }
            | Self::Choice { client_id, .. } => client_id,
        }
    }
}

#[derive(Clone, Serialize)]
struct ClientAttachment {
    filename: String,
    mime_type: String,
    data_base64: Option<String>,
}

impl From<&Attachment> for ClientAttachment {
    fn from(attachment: &Attachment) -> Self {
        use base64::Engine as _;
        Self {
            filename: attachment.filename.clone(),
            mime_type: attachment.mime_type.clone(),
            data_base64: attachment
                .read()
                .ok()
                .map(|data| base64::engine::general_purpose::STANDARD.encode(data)),
        }
    }
}

impl Channel for LocalClient {
    fn name(&self) -> &str {
        CHANNEL
    }

    fn deliver<'a>(&'a self, event: &'a Event) -> DeliveryFuture<'a> {
        Box::pin(async move {
            let (event, conversation, external_id) = match event {
                Event::MessageOut {
                    chat_id: Some(conversation_id),
                    thread_id: Some(client_id),
                    message_id,
                    message:
                        Message::Assistant {
                            text,
                            attachments,
                            tool_calls,
                        },
                    ..
                } => {
                    let id = message_id
                        .clone()
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                    let conversation = conversation(&client_id, &conversation_id);
                    (
                        ClientEvent::Message {
                            client_id: client_id.clone(),
                            conversation_id: conversation_id.clone(),
                            id: id.clone(),
                            content: text.clone().unwrap_or_default(),
                            attachments: attachments.iter().map(ClientAttachment::from).collect(),
                            tool_calls: tool_calls.clone(),
                        },
                        Some(conversation),
                        Some(id),
                    )
                }
                Event::DeleteMessage { message } => {
                    let client_id = message.conversation.thread_id.clone().ok_or_else(|| {
                        "local client conversation requires a client id".to_string()
                    })?;
                    (
                        ClientEvent::Delete {
                            client_id,
                            conversation_id: message.conversation.chat_id.clone(),
                            message_id: message.message_id.clone(),
                        },
                        None,
                        None,
                    )
                }
                Event::ChannelAction {
                    conversation,
                    action,
                } => {
                    let client_id = conversation.thread_id.clone().ok_or_else(|| {
                        "local client conversation requires a client id".to_string()
                    })?;
                    (
                        ClientEvent::Activity {
                            client_id,
                            conversation_id: conversation.chat_id.clone(),
                            action: action.clone(),
                        },
                        None,
                        None,
                    )
                }
                Event::UserInputRequested {
                    conversation,
                    request,
                } => {
                    let client_id = conversation.thread_id.clone().ok_or_else(|| {
                        "local client conversation requires a client id".to_string()
                    })?;
                    (
                        ClientEvent::Choice {
                            client_id,
                            conversation_id: conversation.chat_id.clone(),
                            id: uuid::Uuid::new_v4().to_string(),
                            question: request.question.clone(),
                            options: request.options.clone(),
                        },
                        None,
                        None,
                    )
                }
                _ => return Err("event is not deliverable to the local client".to_string()),
            };
            let _ = self.events.send(event);
            Ok(Delivery {
                conversation,
                external_id,
            })
        })
    }
}

const MIN_FIXED_TOKEN_LEN: usize = 32;
/// The only client the device token (`IRONPAW_DEVICE_TOKEN`) can act as.
pub(crate) const DEVICE_CLIENT_ID: &str = "esp32";

/// The main token opens everything. The device token only opens requests that are about
/// `DEVICE_CLIENT_ID` (or about no client, like speech).
fn authorize(
    headers: &HeaderMap,
    token: &str,
    device_token: Option<&str>,
    client_id: Option<&str>,
) -> Result<(), (StatusCode, String)> {
    use crate::network::server::auth::authorize_bearer;
    let error = match authorize_bearer(headers, token) {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };
    let Some(device_token) = device_token else {
        return Err(error);
    };
    authorize_bearer(headers, device_token).map_err(|_| error)?;
    if client_id.is_some_and(|id| id != DEVICE_CLIENT_ID) {
        return Err((
            StatusCode::FORBIDDEN,
            format!("the device token only works for client {DEVICE_CLIENT_ID}"),
        ));
    }
    Ok(())
}

pub(crate) fn device_token(token: Option<String>) -> Result<Option<String>, String> {
    match token.filter(|token| !token.is_empty()) {
        Some(token) if token.len() < MIN_FIXED_TOKEN_LEN => Err(format!(
            "IRONPAW_DEVICE_TOKEN must be at least {MIN_FIXED_TOKEN_LEN} characters"
        )),
        token => Ok(token),
    }
}

/// A fixed token lets devices that cannot be reconfigured at runtime (the ESP32) stay
/// paired across restarts. It also unlocks Ironpaw, so short tokens are rejected.
pub(crate) fn connection_token(fixed: Option<String>) -> Result<String, String> {
    match fixed.filter(|token| !token.is_empty()) {
        None => Ok(uuid::Uuid::new_v4().to_string()),
        Some(token) if token.len() < MIN_FIXED_TOKEN_LEN => Err(format!(
            "IRONPAW_CLIENT_TOKEN must be at least {MIN_FIXED_TOKEN_LEN} characters"
        )),
        Some(token) => Ok(token),
    }
}

fn conversation(client_id: &str, conversation_id: &str) -> ConversationKey {
    ConversationKey {
        channel: CHANNEL.to_string(),
        chat_id: conversation_id.to_string(),
        thread_id: Some(client_id.to_string()),
    }
}

async fn status(
    State(client): State<Arc<LocalClient>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    client.authorize(&headers, None)?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
struct IncomingMessage {
    client_id: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    message_id: Option<String>,
    #[serde(default)]
    attachments: Vec<IncomingAttachment>,
}

async fn send_message(
    State(client): State<Arc<LocalClient>>,
    RoutePath(conversation_id): RoutePath<String>,
    headers: HeaderMap,
    Json(incoming): Json<IncomingMessage>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    client.authorize(&headers, Some(&incoming.client_id))?;
    validate_id("client", &incoming.client_id)?;
    validate_id("conversation", &conversation_id)?;
    if incoming.content.trim().is_empty() && incoming.attachments.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "message cannot be empty".to_string(),
        ));
    }
    if incoming.attachments.len() > 4 {
        return Err((
            StatusCode::BAD_REQUEST,
            "a message can include at most 4 attachments".to_string(),
        ));
    }

    // Voice clients (the ESP32) send speech; the agent only ever sees the transcript.
    let mut content = vec![incoming.content];
    let mut attachments = Vec::new();
    for attachment in incoming.attachments {
        if attachment.mime_type.starts_with("audio/") {
            content.push(
                client
                    .inference
                    .transcribe(
                        attachment.filename,
                        attachment.mime_type,
                        attachment.data_base64,
                    )
                    .await
                    .unwrap_or_else(|error| format!("Audio transcription failed: {error}")),
            );
        } else {
            attachments.push(attachment);
        }
    }
    content.retain(|text| !text.trim().is_empty());
    let content = content.join("\n\n");
    if content.is_empty() && attachments.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "no speech detected in audio".to_string(),
        ));
    }

    client
        .receive(
            incoming.client_id,
            &conversation_id,
            incoming.message_id,
            content.clone(),
            attachments,
        )
        .map_err(internal_error)?;

    Ok(Json(serde_json::json!({ "ok": true, "content": content })))
}

#[derive(Deserialize)]
struct EventQuery {
    client_id: String,
}

async fn events(
    State(client): State<Arc<LocalClient>>,
    Query(query): Query<EventQuery>,
    headers: HeaderMap,
) -> Result<Sse<impl futures::Stream<Item = Result<SseEvent, Infallible>>>, (StatusCode, String)> {
    client.authorize(&headers, Some(&query.client_id))?;
    validate_id("client", &query.client_id)?;
    let receiver = client.events.subscribe();
    let client_id = query.client_id;
    let events = stream::unfold(receiver, move |mut receiver| {
        let client_id = client_id.clone();
        async move {
            loop {
                match receiver.recv().await {
                    Ok(event) if event.client_id() == client_id => {
                        let event = SseEvent::default().json_data(event).ok()?;
                        return Some((Ok(event), receiver));
                    }
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        }
    });
    let stream =
        stream::once(async { Ok::<_, Infallible>(SseEvent::default().comment("connected")) })
            .chain(events);
    Ok(Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default()))
}

fn validate_id(name: &str, value: &str) -> Result<(), (StatusCode, String)> {
    if !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        Ok(())
    } else {
        Err((StatusCode::BAD_REQUEST, format!("invalid {name} id")))
    }
}

const NOTES_FILE: &str = "notes.md";
const MAX_LISTED_NOTES: usize = 20;

/// Voice notes live in the conversation's file workspace, so the agent can read and edit
/// them with its file tools too ("what were my notes from today?").
fn notes_path(files_dir: &Path, client_id: &str, conversation_id: &str) -> PathBuf {
    files_dir
        .join(crate::message::conversation_workspace(&conversation(
            client_id,
            conversation_id,
        )))
        .join(NOTES_FILE)
}

/// Joins `content` and the transcript of every audio attachment into one line.
async fn transcript(
    client: &LocalClient,
    incoming: IncomingMessage,
) -> Result<String, (StatusCode, String)> {
    let mut text = vec![incoming.content];
    for attachment in incoming.attachments {
        if attachment.mime_type.starts_with("audio/") {
            text.push(
                client
                    .inference
                    .transcribe(
                        attachment.filename,
                        attachment.mime_type,
                        attachment.data_base64,
                    )
                    .await
                    .map_err(internal_error)?,
            );
        }
    }
    let text = text
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if text.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "no speech detected in audio".to_string(),
        ));
    }
    Ok(text)
}

/// Speech to text only. Voice clients (the ESP32) decide what the words are for:
/// a message to the agent, a note, or a command of their own.
async fn transcribe(
    State(client): State<Arc<LocalClient>>,
    headers: HeaderMap,
    Json(incoming): Json<IncomingMessage>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    client.authorize(&headers, Some(&incoming.client_id))?;
    validate_id("client", &incoming.client_id)?;
    let text = transcript(&client, incoming).await?;
    Ok(Json(serde_json::json!({ "text": text })))
}

#[derive(Deserialize)]
struct NotesQuery {
    client_id: String,
}

/// The latest notes, newest first, as written in `notes.md` (the agent may have edited it).
async fn list_notes(
    State(client): State<Arc<LocalClient>>,
    RoutePath(conversation_id): RoutePath<String>,
    Query(query): Query<NotesQuery>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    client.authorize(&headers, Some(&query.client_id))?;
    validate_id("client", &query.client_id)?;
    validate_id("conversation", &conversation_id)?;
    let path = notes_path(&client.files_dir, &query.client_id, &conversation_id);
    let text = match tokio::fs::read_to_string(&path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(internal_error(format!("cannot read notes: {error}"))),
    };
    Ok(Json(serde_json::json!({ "notes": latest_notes(&text) })))
}

fn latest_notes(text: &str) -> Vec<&str> {
    text.lines()
        .map(|line| line.trim().trim_start_matches("- ").trim())
        .filter(|line| !line.is_empty())
        .rev()
        .take(MAX_LISTED_NOTES)
        .collect()
}

pub(crate) const EVENTS_FILE: &str = "events.json";
/// Events this far in the past are still sent, so a board that was asleep at the minute
/// still rings (it only fires what is at most `FIRE_WINDOW_SECS` old; see the firmware).
const PAST_EVENTS_MINUTES: i64 = 2;
const MAX_LISTED_EVENTS: usize = 50;

/// One reminder in `events.json`. `at` is local time without a zone, the way the agent
/// writes it; `repeat` is "daily" or "weekly".
#[derive(Clone, Deserialize, Serialize, PartialEq, Debug, schemars::JsonSchema)]
pub(crate) struct ReminderEvent {
    pub at: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat: Option<String>,
}

pub(crate) const EVENT_TIME: &str = "%Y-%m-%dT%H:%M";
/// The conversation the board talks in (`CONVERSATION_ID` in the firmware).
const DEVICE_CONVERSATION_ID: &str = "esp32";

/// The board's `events.json`, so tools in any conversation can set its reminders.
pub(crate) fn device_events_path(files_dir: &Path) -> PathBuf {
    notes_path(files_dir, DEVICE_CLIENT_ID, DEVICE_CONVERSATION_ID).with_file_name(EVENTS_FILE)
}

/// Parses `events.json`; a broken file counts as empty (the agent edits it).
pub(crate) fn read_events(text: &str) -> Vec<ReminderEvent> {
    serde_json::from_str(text).unwrap_or_else(|error| {
        eprintln!("events.json is not a list of events: {error}");
        Vec::new()
    })
}

/// The events worth sending to the board: repeats rolled forward to their next occurrence,
/// anything older than `PAST_EVENTS_MINUTES` dropped, sorted by time. Malformed entries are
/// skipped with a log line rather than failing the whole list (the agent edits the file).
fn upcoming_events(events: Vec<ReminderEvent>, now: chrono::NaiveDateTime) -> Vec<ReminderEvent> {
    let floor = now - chrono::Duration::minutes(PAST_EVENTS_MINUTES);
    let mut out: Vec<(chrono::NaiveDateTime, ReminderEvent)> = events
        .into_iter()
        .filter_map(|mut event| {
            let mut at = chrono::NaiveDateTime::parse_from_str(&event.at, EVENT_TIME)
                .map_err(|error| eprintln!("event {:?} has a bad time: {error}", event.at))
                .ok()?;
            let step = match event.repeat.as_deref() {
                Some("daily") => Some(chrono::Duration::days(1)),
                Some("weekly") => Some(chrono::Duration::weeks(1)),
                _ => None,
            };
            if let Some(step) = step {
                while at < floor {
                    at += step;
                }
            }
            (at >= floor).then(|| {
                event.at = at.format(EVENT_TIME).to_string();
                (at, event)
            })
        })
        .collect();
    out.sort_by_key(|(at, _)| *at);
    out.into_iter()
        .map(|(_, event)| event)
        .take(MAX_LISTED_EVENTS)
        .collect()
}

/// Upcoming reminders from `events.json` in the conversation's workspace, plus the current
/// time so the board can set its clock: `now` is Unix UTC, `offset` the local zone in seconds.
/// The board rings them itself (and keeps a copy on its SD card for when it is offline).
async fn list_events(
    State(client): State<Arc<LocalClient>>,
    RoutePath(conversation_id): RoutePath<String>,
    Query(query): Query<NotesQuery>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    client.authorize(&headers, Some(&query.client_id))?;
    validate_id("client", &query.client_id)?;
    validate_id("conversation", &conversation_id)?;
    let path = notes_path(&client.files_dir, &query.client_id, &conversation_id)
        .with_file_name(EVENTS_FILE);
    let text = match tokio::fs::read_to_string(&path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "[]".to_string(),
        Err(error) => return Err(internal_error(format!("cannot read events: {error}"))),
    };
    let mut events = read_events(&text);
    if query.client_id == DEVICE_CLIENT_ID {
        events.extend(calendar_events().await);
    }
    let now = chrono::Local::now();
    Ok(Json(serde_json::json!({
        "now": now.timestamp(),
        "offset": now.offset().local_minus_utc(),
        "events": upcoming_events(events, now.naive_local()),
    })))
}

/// The board rings a calendar event this long before it starts.
const CALENDAR_LEAD_MINUTES: i64 = 5;
/// Google's feed holds every past event too; years of them fit well under this.
const MAX_CALENDAR_BYTES: usize = 10_000_000;

/// Events from the calendar at `IRONPAW_CALENDAR_ICS` (Google Calendar's secret iCal
/// address), fetched on every board refresh. Unset or unreachable means none.
// ponytail: refetched each time (the board asks every 10 min); cache it if the feed is huge.
async fn calendar_events() -> Vec<ReminderEvent> {
    let Ok(url) = std::env::var("IRONPAW_CALENDAR_ICS") else {
        return Vec::new();
    };
    // Errors never include the URL, which is a secret.
    match crate::network::public::PublicHttp::new()
        .get(&url, MAX_CALENDAR_BYTES, true)
        .await
    {
        Ok(response) if response.status == 200 => {
            ics_events(&String::from_utf8_lossy(&response.body))
        }
        Ok(response) => {
            eprintln!("cannot fetch the calendar: HTTP {}", response.status);
            Vec::new()
        }
        Err(error) => {
            eprintln!("cannot fetch the calendar: {error}");
            Vec::new()
        }
    }
}

/// One reminder per timed `VEVENT`, `CALENDAR_LEAD_MINUTES` before it starts.
// ponytail: all-day events and RRULE repeats are skipped (only each event's first
// start is read), and TZID times are taken as Ironpaw's local time. Use an iCal crate
// if the calendar lives in another zone or recurring meetings matter.
fn ics_events(text: &str) -> Vec<ReminderEvent> {
    let unfolded = text
        .replace("\r\n ", "")
        .replace("\n ", "")
        .replace("\r\n\t", "");
    let mut events = Vec::new();
    let (mut start, mut summary) = (None, None);
    for line in unfolded.lines() {
        let line = line.trim_end_matches('\r');
        if line == "BEGIN:VEVENT" {
            (start, summary) = (None, None);
        } else if line == "END:VEVENT" {
            if let (Some(at), Some(text)) = (start.take(), summary.take()) {
                let at: chrono::NaiveDateTime =
                    at - chrono::Duration::minutes(CALENDAR_LEAD_MINUTES);
                events.push(ReminderEvent {
                    at: at.format(EVENT_TIME).to_string(),
                    text,
                    repeat: None,
                });
            }
        } else if let Some((name, value)) = line.split_once(':') {
            let name = name.split(';').next().unwrap_or(name);
            if name == "DTSTART" {
                start = ics_time(value);
            } else if name == "SUMMARY" {
                summary = Some(
                    value
                        .replace("\\n", " ")
                        .replace("\\,", ",")
                        .replace("\\;", ";")
                        .replace("\\\\", "\\"),
                );
            }
        }
    }
    events
}

/// `20261105T140000Z` (UTC) or `20261105T110000` (local); dates without a time are `None`.
fn ics_time(value: &str) -> Option<chrono::NaiveDateTime> {
    if let Some(utc) = value.strip_suffix('Z') {
        let utc = chrono::NaiveDateTime::parse_from_str(utc, "%Y%m%dT%H%M%S").ok()?;
        return Some(utc.and_utc().with_timezone(&chrono::Local).naive_local());
    }
    chrono::NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%S").ok()
}

/// Sample rate of the audio `/client/speech` returns: the same one voice clients record at.
const SPEECH_RATE: u32 = 16_000;
const MAX_SPEECH_CHARS: usize = 4_000;

#[derive(Deserialize)]
struct SpeechRequest {
    text: String,
}

/// Text to speech with a local Piper voice, for clients with a speaker (the ESP32).
/// Returns raw mono s16le PCM at `SPEECH_RATE`, streamed as Piper writes it (sentence by
/// sentence), so a long reply starts playing early. Off unless `IRONPAW_PIPER_MODEL` is set.
async fn speech(
    State(client): State<Arc<LocalClient>>,
    headers: HeaderMap,
    Json(request): Json<SpeechRequest>,
) -> Result<impl axum::response::IntoResponse, (StatusCode, String)> {
    client.authorize(&headers, None)?;
    let Ok(model) = std::env::var("IRONPAW_PIPER_MODEL") else {
        return Err((
            StatusCode::NOT_FOUND,
            "speech is not configured".to_string(),
        ));
    };
    if request.text.trim().is_empty() || request.text.chars().count() > MAX_SPEECH_CHARS {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("text must have 1 to {MAX_SPEECH_CHARS} characters"),
        ));
    }
    let piper = std::env::var("IRONPAW_PIPER").unwrap_or_else(|_| "piper".to_string());
    let audio = piper_speak(&piper, &model, &request.text)
        .await
        .map_err(internal_error)?;
    Ok((
        [(
            axum::http::header::CONTENT_TYPE,
            "audio/L16; rate=16000; channels=1",
        )],
        axum::body::Body::from_stream(audio),
    ))
}

// ponytail: one Piper process per request, so the model loads every time (~1 s).
// Keep a resident Piper if that first second matters.
async fn piper_speak(
    piper: &str,
    model: &str,
    text: &str,
) -> Result<impl futures::Stream<Item = std::io::Result<Vec<u8>>> + use<>, String> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let config = tokio::fs::read(format!("{model}.json"))
        .await
        .map_err(|error| format!("cannot read voice config: {error}"))?;
    let config: serde_json::Value =
        serde_json::from_slice(&config).map_err(|error| format!("bad voice config: {error}"))?;
    let rate = config["audio"]["sample_rate"]
        .as_u64()
        .ok_or("voice config has no audio.sample_rate")? as u32;

    let mut child = tokio::process::Command::new(piper)
        .args(["--model", model, "--output-raw"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("cannot start piper: {error}"))?;
    let mut stdin = child.stdin.take().ok_or("piper has no stdin")?;
    let mut stdout = child.stdout.take().ok_or("piper has no stdout")?;
    // Piper reads one utterance per line.
    let text = text.replace('\n', " ");
    stdin
        .write_all(text.as_bytes())
        .await
        .map_err(|error| format!("cannot write to piper: {error}"))?;
    drop(stdin);

    // Wait for the first audio before answering: if Piper fails, that's a proper error
    // instead of an empty 200.
    let mut buf = vec![0u8; 8192];
    let n = stdout
        .read(&mut buf)
        .await
        .map_err(|error| format!("cannot read piper: {error}"))?;
    if n == 0 {
        let output = child
            .wait_with_output()
            .await
            .map_err(|error| format!("piper failed: {error}"))?;
        return Err(format!(
            "piper failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let mut resampler = Resampler::new(rate, SPEECH_RATE);
    let first = resampler.push(&buf[..n]);
    let rest = stream::unfold(
        (stdout, resampler, buf, child),
        |(mut stdout, mut resampler, mut buf, child)| async move {
            match stdout.read(&mut buf).await {
                Ok(0) => None,
                Ok(n) => {
                    let pcm = resampler.push(&buf[..n]);
                    Some((Ok(pcm), (stdout, resampler, buf, child)))
                }
                Err(error) => Some((Err(error), (stdout, resampler, buf, child))),
            }
        },
    );
    Ok(stream::once(async { Ok(first) }).chain(rest))
}

/// Streaming linear resampler for s16le bytes as they come out of Piper, in chunks of any
/// size (even ones that split a sample in half).
// ponytail: linear interpolation without a low-pass filter. Piper voices are 22.05 kHz, so
// only 8-11 kHz aliases back, and speech has little energy there. Filter if it sounds harsh.
struct Resampler {
    from: u64,
    to: u64,
    /// Position in `pending`, in units of 1/`to` of a sample: exact, so chunking never shifts it.
    position: u64,
    pending: Vec<i16>,
    odd_byte: Option<u8>,
}

impl Resampler {
    fn new(from: u32, to: u32) -> Self {
        Self {
            from: from as u64,
            to: to as u64,
            position: 0,
            pending: Vec::new(),
            odd_byte: None,
        }
    }

    fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut bytes = bytes;
        if let Some(low) = self.odd_byte.take() {
            let Some((&high, rest)) = bytes.split_first() else {
                self.odd_byte = Some(low);
                return Vec::new();
            };
            self.pending.push(i16::from_le_bytes([low, high]));
            bytes = rest;
        }
        let pairs = bytes.chunks_exact(2);
        self.odd_byte = pairs.remainder().first().copied();
        self.pending
            .extend(pairs.map(|b| i16::from_le_bytes([b[0], b[1]])));
        let mut out = Vec::new();
        while ((self.position / self.to) as usize) + 1 < self.pending.len() {
            let index = (self.position / self.to) as usize;
            let fraction = (self.position % self.to) as f64 / self.to as f64;
            let a = self.pending[index] as f64;
            let b = self.pending[index + 1] as f64;
            let sample = (a + (b - a) * fraction).round() as i16;
            out.extend_from_slice(&sample.to_le_bytes());
            self.position += self.from;
        }
        let used = ((self.position / self.to) as usize).min(self.pending.len());
        self.pending.drain(..used);
        self.position -= used as u64 * self.to;
        out
    }
}

fn internal_error(error: String) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_notes_are_newest_first_without_bullets() {
        let text = "- 2026-10-29 10:00 buy milk\n\n- 2026-10-29 11:00 call Ana\n";
        assert_eq!(
            latest_notes(text),
            vec!["2026-10-29 11:00 call Ana", "2026-10-29 10:00 buy milk"]
        );
    }

    #[test]
    fn device_token_only_acts_as_its_client() {
        let bearer = |token: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(
                axum::http::header::AUTHORIZATION,
                format!("Bearer {token}").parse().unwrap(),
            );
            headers
        };
        let (main, device) = ("m".repeat(32), "d".repeat(32));
        let check = |token: &str, device_token, client| {
            authorize(&bearer(token), &main, device_token, client).map_err(|(code, _)| code)
        };
        assert_eq!(check(&main, Some(&device), Some("webui")), Ok(()));
        assert_eq!(check(&device, Some(&device), Some("esp32")), Ok(()));
        assert_eq!(check(&device, Some(&device), None), Ok(()));
        assert_eq!(
            check(&device, Some(&device), Some("webui")),
            Err(StatusCode::FORBIDDEN)
        );
        assert_eq!(
            check(&device, None, Some("esp32")),
            Err(StatusCode::UNAUTHORIZED)
        );
        assert!(device_token(Some("short".into())).is_err());
    }

    #[test]
    fn upcoming_events_roll_repeats_and_sort() {
        let now = chrono::NaiveDateTime::parse_from_str("2026-11-02T10:00", EVENT_TIME).unwrap();
        let text = r#"[
            {"at":"2026-11-03T09:00","text":"dentist"},
            {"at":"2026-10-01T07:30","text":"pills","repeat":"daily"},
            {"at":"2026-11-02T09:59","text":"just rang"},
            {"at":"2026-11-02T09:00","text":"missed"},
            {"at":"soon","text":"broken"}
        ]"#;
        let got: Vec<(String, String)> = upcoming_events(read_events(text), now)
            .into_iter()
            .map(|e| (e.at, e.text))
            .collect();
        assert_eq!(
            got,
            vec![
                ("2026-11-02T09:59".to_string(), "just rang".to_string()),
                ("2026-11-03T07:30".to_string(), "pills".to_string()),
                ("2026-11-03T09:00".to_string(), "dentist".to_string()),
            ]
        );
        assert!(read_events("not json").is_empty());
    }

    #[test]
    fn ics_events_ring_before_timed_events() {
        let text = "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nDTSTART;TZID=America/Argentina/Buenos_Aires:\
            20261105T110000\r\nSUMMARY:Standup\\, team\r\n  sync\r\nEND:VEVENT\r\n\
            BEGIN:VEVENT\r\nDTSTART;VALUE=DATE:20261106\r\nSUMMARY:Holiday\r\nEND:VEVENT\r\n\
            BEGIN:VEVENT\r\nDTSTART:20261107T150000Z\r\nSUMMARY:Call\r\nEND:VEVENT\r\n\
            END:VCALENDAR\r\n";
        let got: Vec<(String, String)> = ics_events(text)
            .into_iter()
            .map(|e| (e.at, e.text))
            .collect();
        let call = ics_time("20261107T150000Z").unwrap() - chrono::Duration::minutes(5);
        assert_eq!(
            got,
            vec![
                (
                    "2026-11-05T10:55".to_string(),
                    "Standup, team sync".to_string()
                ),
                (call.format(EVENT_TIME).to_string(), "Call".to_string()),
            ]
        );
    }

    #[test]
    fn resampler_keeps_duration_and_ignores_chunk_boundaries() {
        let bytes: Vec<u8> = (0..22_050i16)
            .flat_map(|i| (i % 100).to_le_bytes())
            .collect();
        let whole = Resampler::new(22_050, 16_000).push(&bytes);
        assert!((whole.len() / 2).abs_diff(16_000) <= 1);
        let mut chunked = Resampler::new(22_050, 16_000);
        let pieces: Vec<u8> = bytes
            .chunks(333)
            .flat_map(|chunk| chunked.push(chunk))
            .collect();
        assert_eq!(pieces, whole);
        let up: Vec<u8> = [0i16, 100].iter().flat_map(|s| s.to_le_bytes()).collect();
        let samples: Vec<i16> = Resampler::new(2, 4)
            .push(&up)
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(samples, vec![0, 50]);
    }

    #[test]
    fn fixed_connection_tokens_must_be_long() {
        assert!(connection_token(Some("short".to_string())).is_err());
        let fixed = "a".repeat(MIN_FIXED_TOKEN_LEN);
        assert_eq!(connection_token(Some(fixed.clone())).unwrap(), fixed);
        assert_eq!(connection_token(Some(String::new())).unwrap().len(), 36);
        assert_eq!(connection_token(None).unwrap().len(), 36);
    }

    #[test]
    fn client_ids_are_safe_route_components() {
        assert!(validate_id("client", "e7438f43-34b3_4f2b").is_ok());
        assert!(validate_id("client", "../other").is_err());
    }
}
