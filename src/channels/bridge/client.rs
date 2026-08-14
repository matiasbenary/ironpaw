use std::{convert::Infallible, sync::Arc};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response, Sse, sse::Event as SseEvent},
    routing::{get, post},
};
use futures::{StreamExt as _, stream};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::{
    bus::{Bus, ChannelAction, ConversationKey, Event},
    channels::{append_inbound_message, bridge::history::append_channel_history},
    history::History,
    message::{Attachment, Message, ToolCall},
};

use super::{BridgeState, auth::authorize_bearer, inbound::attachment_from_content};

const CHANNEL: &str = "local-client";

pub(super) fn routes() -> Router<BridgeState> {
    Router::new()
        .route(
            "/client/conversations/{conversation_id}/messages",
            post(send_message).options(preflight),
        )
        .route("/client/events", get(events).options(preflight))
        .route("/client/status", get(status).options(preflight))
        .route("/client/oauth/status", get(oauth_status).options(preflight))
        .layer(DefaultBodyLimit::max(56 * 1024 * 1024))
        .layer(middleware::from_fn(cors))
}

#[derive(Deserialize)]
struct OauthStatusQuery {
    state: String,
}

async fn oauth_status(
    State(state): State<BridgeState>,
    Query(query): Query<OauthStatusQuery>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    authorize_bearer(&headers, &state.token)?;
    Ok(Json(serde_json::json!({
        "complete": crate::network::oauth::callback_received(&query.state)
    })))
}

async fn status(
    State(state): State<BridgeState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    authorize_bearer(&headers, &state.token)?;
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

#[derive(Deserialize)]
struct IncomingAttachment {
    filename: String,
    mime_type: String,
    data_base64: String,
}

async fn send_message(
    State(state): State<BridgeState>,
    Path(conversation_id): Path<String>,
    headers: HeaderMap,
    Json(incoming): Json<IncomingMessage>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    authorize_bearer(&headers, &state.token)?;
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

    let mut text_parts = Vec::new();
    if !incoming.content.trim().is_empty() {
        text_parts.push(incoming.content);
    }
    let mut attachments = Vec::new();
    for attachment in incoming.attachments {
        match attachment_from_content(
            Some(attachment.filename),
            attachment.mime_type,
            attachment.data_base64,
            &state.files_dir,
        ) {
            Ok(attachment) => attachments.push(attachment),
            Err(error) => text_parts.push(error),
        }
    }

    let conversation = conversation(&incoming.client_id, &conversation_id);
    let message = Message::User {
        text: text_parts.join("\n\n"),
        attachments,
    };
    append_inbound_message(
        state.history.as_ref(),
        &conversation,
        incoming.message_id.clone(),
        &message,
    )
    .map_err(internal_error)?;
    state.bus.publish(Event::MessageIn {
        sender: crate::bus::Sender::User(incoming.client_id),
        conversation,
        message_id: incoming.message_id,
        message,
    });

    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
struct EventQuery {
    client_id: String,
}

async fn events(
    State(state): State<BridgeState>,
    Query(query): Query<EventQuery>,
    headers: HeaderMap,
) -> Result<Sse<impl futures::Stream<Item = Result<SseEvent, Infallible>>>, (StatusCode, String)> {
    authorize_bearer(&headers, &state.token)?;
    validate_id("client", &query.client_id)?;
    let receiver = state.client_events.subscribe();
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

#[derive(Clone, Serialize)]
#[serde(tag = "kind")]
pub(crate) enum ClientEvent {
    #[serde(rename = "message")]
    Message {
        client_id: String,
        conversation_id: String,
        id: String,
        content: String,
        attachments: Vec<ClientAttachment>,
        tool_calls: Vec<ToolCall>,
    },
    #[serde(rename = "delete")]
    Delete {
        client_id: String,
        conversation_id: String,
        message_id: String,
    },
    #[serde(rename = "activity")]
    Activity {
        client_id: String,
        conversation_id: String,
        action: ChannelAction,
    },
    #[serde(rename = "choice")]
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
pub(crate) struct ClientAttachment {
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

pub(super) async fn forward_events(
    bus: Bus,
    history: Arc<dyn History>,
    sender: broadcast::Sender<ClientEvent>,
) {
    let mut events = bus.subscribe();
    loop {
        let event = match events.recv().await {
            Ok(event) => event,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return,
        };
        let client_event = match event {
            Event::MessageOut {
                channel,
                chat_id: Some(conversation_id),
                thread_id: Some(client_id),
                message_id,
                message:
                    Message::Assistant {
                        text,
                        attachments,
                        tool_calls,
                    },
            } if channel == CHANNEL => {
                let id = message_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                let conversation = conversation(&client_id, &conversation_id);
                let message = Message::Assistant {
                    text: text.clone(),
                    attachments: attachments.clone(),
                    tool_calls: tool_calls.clone(),
                };
                if let Err(error) = append_channel_history(
                    history.as_ref(),
                    &conversation,
                    Some(id.clone()),
                    &message,
                ) {
                    eprintln!("failed to persist local client history: {error}");
                }
                ClientEvent::Message {
                    client_id,
                    conversation_id,
                    id,
                    content: text.unwrap_or_default(),
                    attachments: attachments.iter().map(ClientAttachment::from).collect(),
                    tool_calls,
                }
            }
            Event::DeleteMessage { message }
                if message.conversation.channel == CHANNEL
                    && message.conversation.thread_id.is_some() =>
            {
                if let Err(error) = history.delete_external_message(&message) {
                    eprintln!("failed to delete local client history: {error}");
                }
                ClientEvent::Delete {
                    client_id: message.conversation.thread_id.unwrap(),
                    conversation_id: message.conversation.chat_id,
                    message_id: message.message_id,
                }
            }
            Event::ChannelAction {
                conversation,
                action,
            } if conversation.channel == CHANNEL && conversation.thread_id.is_some() => {
                ClientEvent::Activity {
                    client_id: conversation.thread_id.unwrap(),
                    conversation_id: conversation.chat_id,
                    action,
                }
            }
            Event::UserInputRequested {
                conversation,
                request,
            } if conversation.channel == CHANNEL && conversation.thread_id.is_some() => {
                ClientEvent::Choice {
                    client_id: conversation.thread_id.unwrap(),
                    conversation_id: conversation.chat_id,
                    id: uuid::Uuid::new_v4().to_string(),
                    question: request.question,
                    options: request.options,
                }
            }
            _ => continue,
        };
        let _ = sender.send(client_event);
    }
}

fn conversation(client_id: &str, conversation_id: &str) -> ConversationKey {
    ConversationKey {
        channel: CHANNEL.to_string(),
        chat_id: conversation_id.to_string(),
        thread_id: Some(client_id.to_string()),
    }
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

fn internal_error(error: String) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, error)
}

async fn preflight() -> StatusCode {
    StatusCode::NO_CONTENT
}

async fn cors(request: Request, next: Next) -> Response {
    let is_preflight = request.method() == Method::OPTIONS;
    let mut response = if is_preflight {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(request).await
    };
    let headers = response.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("authorization, content-type"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_ids_are_safe_route_components() {
        assert!(validate_id("client", "e7438f43-34b3_4f2b").is_ok());
        assert!(validate_id("client", "../other").is_err());
    }
}
