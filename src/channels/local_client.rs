use std::{convert::Infallible, path::Path, path::PathBuf, sync::Arc};

use axum::{
    Json, Router,
    extract::{Path as RoutePath, Query, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response, Sse, sse::Event as SseEvent},
    routing::{get, post},
};
use futures::{StreamExt as _, stream};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::{
    bus::{ChannelAction, ConversationKey, Event},
    channels::bridge::{Bridge, Channel, Delivery, DeliveryFuture, InboundMessage},
    message::{Attachment, Message, ToolCall},
};

use super::wasm::attachment_from_content;

pub(crate) const CHANNEL: &str = "local-client";

pub(crate) struct LocalClient {
    bridge: Bridge,
    files_dir: PathBuf,
    token: String,
    events: broadcast::Sender<ClientEvent>,
}

impl LocalClient {
    pub(crate) fn new(bridge: Bridge, files_dir: PathBuf) -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        Arc::new(Self {
            bridge,
            files_dir,
            token: uuid::Uuid::new_v4().to_string(),
            events,
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
            .route("/client/oauth/status", get(oauth_status).options(preflight))
            .layer(middleware::from_fn(cors))
            .with_state(Arc::clone(self))
    }

    fn authorize(&self, headers: &HeaderMap) -> Result<(), (StatusCode, String)> {
        crate::network::server::auth::authorize_bearer(headers, &self.token)
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

fn conversation(client_id: &str, conversation_id: &str) -> ConversationKey {
    ConversationKey {
        channel: CHANNEL.to_string(),
        chat_id: conversation_id.to_string(),
        thread_id: Some(client_id.to_string()),
    }
}

#[derive(Deserialize)]
struct OauthStatusQuery {
    state: String,
}

async fn oauth_status(
    State(client): State<Arc<LocalClient>>,
    Query(query): Query<OauthStatusQuery>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    client.authorize(&headers)?;
    Ok(Json(serde_json::json!({
        "complete": crate::network::oauth::callback_received(&query.state)
    })))
}

async fn status(
    State(client): State<Arc<LocalClient>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    client.authorize(&headers)?;
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
    client.authorize(&headers)?;
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

    client
        .receive(
            incoming.client_id,
            &conversation_id,
            incoming.message_id,
            incoming.content,
            incoming.attachments,
        )
        .map_err(internal_error)?;

    Ok(Json(serde_json::json!({ "ok": true })))
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
    client.authorize(&headers)?;
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
