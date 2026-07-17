use axum::{Json, Router, extract::State, routing::post};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    crossterm::{
        event::{
            self, DisableMouseCapture, EnableMouseCapture, Event as CtEvent, KeyCode, KeyEvent,
            KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
        },
        execute,
        terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
    },
    layout::{Constraint, Direction, Layout},
    text::{Line, Text},
    widgets::{Block, Borders, Paragraph},
};
use serde::Deserialize;
use std::{
    io::{self, Stdout},
    time::Duration,
};
use tokio::sync::mpsc;

const DEFAULT_ADDR: &str = "127.0.0.1:7878";

/// A single line in the conversation transcript. `id` is set for messages
/// that can later be targeted by a `DeleteMessage` event (the user's own
/// submissions, and assistant replies that the model chooses to delete
/// later), so a sensitive entry (e.g. an unlock phrase) can be removed from
/// the transcript once it has been consumed.
struct DisplayMessage {
    id: Option<String>,
    text: String,
}

enum AppEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    Callback(CallbackPayload),
}

#[derive(Deserialize)]
#[serde(tag = "kind")]
enum CallbackPayload {
    #[serde(rename = "message")]
    Message {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        content: String,
        #[serde(default)]
        tool_calls: Vec<serde_json::Value>,
    },
    #[serde(rename = "delete")]
    Delete { message_id: String },
}

#[tokio::main]
async fn main() -> io::Result<()> {
    let addr = std::env::var("IRONPAW_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());
    let token = std::env::args().nth(1).unwrap_or_default();

    run(&addr, &token).await
}

async fn run(addr: &str, token: &str) -> io::Result<()> {
    if token.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "usage: ironpaw-tui <token>",
        ));
    }

    let (events_tx, events_rx) = mpsc::channel::<AppEvent>(64);
    let client = reqwest::Client::new();
    let url = bridge_url(addr);
    let callback_url = start_callback_server(events_tx.clone()).await?;
    spawn_key_poller(events_tx.clone());

    let mut terminal = enter_terminal()?;
    let result = run_app(
        &mut terminal,
        events_rx,
        events_tx,
        client,
        url,
        token.to_string(),
        callback_url,
    )
    .await;
    leave_terminal(terminal)?;
    result
}

async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut events_rx: mpsc::Receiver<AppEvent>,
    events_tx: mpsc::Sender<AppEvent>,
    client: reqwest::Client,
    url: String,
    token: String,
    callback_url: String,
) -> io::Result<()> {
    let mut messages: Vec<DisplayMessage> = Vec::new();
    let mut input = String::new();
    let mut next_message_id: u32 = 0;
    let mut scroll_from_bottom: usize = 0;

    loop {
        terminal.draw(|frame| draw(frame, &messages, &input, &mut scroll_from_bottom))?;

        let Some(event) = events_rx.recv().await else {
            return Ok(());
        };

        match event {
            AppEvent::Key(key) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                    return Ok(());
                }
                match key.code {
                    KeyCode::Enter => {
                        let content = input.trim().to_string();
                        input.clear();
                        if content.is_empty() {
                            continue;
                        }
                        if content == "/quit" {
                            return Ok(());
                        }
                        let id = {
                            next_message_id += 1;
                            format!("user-{next_message_id}")
                        };
                        messages.push(DisplayMessage {
                            id: Some(id.clone()),
                            text: format!("you> {content}"),
                        });
                        scroll_from_bottom = 0;
                        spawn_send(
                            client.clone(),
                            url.clone(),
                            token.clone(),
                            callback_url.clone(),
                            content,
                            id,
                            events_tx.clone(),
                        );
                    }
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::Up => {
                        scroll_from_bottom = scroll_from_bottom.saturating_add(1);
                    }
                    KeyCode::Down => {
                        scroll_from_bottom = scroll_from_bottom.saturating_sub(1);
                    }
                    KeyCode::PageUp => {
                        scroll_from_bottom = scroll_from_bottom.saturating_add(10);
                    }
                    KeyCode::PageDown => {
                        scroll_from_bottom = scroll_from_bottom.saturating_sub(10);
                    }
                    KeyCode::Home => {
                        scroll_from_bottom = usize::MAX;
                    }
                    KeyCode::End => {
                        scroll_from_bottom = 0;
                    }
                    KeyCode::Char(c) => input.push(c),
                    _ => {}
                }
            }
            AppEvent::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => {
                    scroll_from_bottom = scroll_from_bottom.saturating_add(3);
                }
                MouseEventKind::ScrollDown => {
                    scroll_from_bottom = scroll_from_bottom.saturating_sub(3);
                }
                _ => {}
            },
            AppEvent::Callback(CallbackPayload::Message {
                id,
                content,
                tool_calls,
            }) => {
                messages.push(DisplayMessage {
                    id,
                    text: format!("ironpaw> {}", display_content(&content, &tool_calls)),
                });
            }
            AppEvent::Callback(CallbackPayload::Delete { message_id }) => {
                if let Some(message) = messages
                    .iter_mut()
                    .find(|message| message.id.as_deref() == Some(message_id.as_str()))
                {
                    let prefix = message
                        .text
                        .split_once(' ')
                        .map_or("", |(prefix, _)| prefix);
                    message.text = format!("{prefix} [redacted]");
                    message.id = None;
                }
            }
        }
    }
}

fn draw(
    frame: &mut Frame,
    messages: &[DisplayMessage],
    input: &str,
    scroll_from_bottom: &mut usize,
) {
    let area = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(3)])
        .split(frame.area());

    let conversation_area = area[0];
    let text_width = conversation_area.width.saturating_sub(2) as usize;
    let wrapped: Vec<Line> = messages
        .iter()
        .flat_map(|message| wrap_text(&message.text, text_width))
        .map(Line::raw)
        .collect();
    let visible_height = conversation_area.height.saturating_sub(2) as usize;
    let max_scroll = wrapped.len().saturating_sub(visible_height);
    *scroll_from_bottom = (*scroll_from_bottom).min(max_scroll);
    let scroll = scroll_offset(wrapped.len(), visible_height, *scroll_from_bottom);
    let conversation = Paragraph::new(Text::from(wrapped))
        .block(Block::default().borders(Borders::ALL).title("ironpaw"))
        .scroll((scroll, 0));
    frame.render_widget(conversation, conversation_area);

    let prompt = "you> ";
    let input_line = Paragraph::new(format!("{prompt}{input}"))
        .block(Block::default().borders(Borders::ALL).title("input"));
    frame.render_widget(input_line, area[1]);
    frame.set_cursor_position((
        area[1].x + 1 + prompt.len() as u16 + input.len() as u16,
        area[1].y + 1,
    ));
}

/// Greedily word-wraps `text` to `width` columns so the caller can know the
/// exact number of rendered rows up front (needed to compute a scroll offset
/// that pins the view to the bottom, since Paragraph's own wrapping doesn't
/// expose a row count before rendering).
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for raw_line in text.split('\n') {
        if raw_line.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut current = String::new();
        for word in raw_line.split(' ') {
            let candidate_len = if current.is_empty() {
                word.chars().count()
            } else {
                current.chars().count() + 1 + word.chars().count()
            };
            if candidate_len > width && !current.is_empty() {
                lines.push(std::mem::take(&mut current));
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
            while current.chars().count() > width {
                let split_at = current
                    .char_indices()
                    .nth(width)
                    .map(|(idx, _)| idx)
                    .unwrap_or(current.len());
                let rest = current.split_off(split_at);
                lines.push(std::mem::take(&mut current));
                current = rest;
            }
        }
        lines.push(current);
    }
    lines
}

fn scroll_offset(content_height: usize, visible_height: usize, scroll_from_bottom: usize) -> u16 {
    let max_scroll = content_height.saturating_sub(visible_height);
    max_scroll
        .saturating_sub(scroll_from_bottom.min(max_scroll))
        .min(u16::MAX as usize) as u16
}

fn spawn_key_poller(events_tx: mpsc::Sender<AppEvent>) {
    tokio::spawn(async move {
        loop {
            let event = tokio::task::spawn_blocking(|| {
                if event::poll(Duration::from_millis(100)).unwrap_or(false) {
                    event::read().ok()
                } else {
                    None
                }
            })
            .await
            .ok()
            .flatten();

            match event {
                Some(CtEvent::Key(key)) => {
                    if events_tx.send(AppEvent::Key(key)).await.is_err() {
                        break;
                    }
                }
                Some(CtEvent::Mouse(mouse)) => {
                    if events_tx.send(AppEvent::Mouse(mouse)).await.is_err() {
                        break;
                    }
                }
                _ => {}
            }
        }
    });
}

fn spawn_send(
    client: reqwest::Client,
    url: String,
    token: String,
    callback_url: String,
    content: String,
    message_id: String,
    events_tx: mpsc::Sender<AppEvent>,
) {
    tokio::spawn(async move {
        let response = client
            .post(&url)
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "content": content,
                "callback_url": callback_url,
                "message_id": message_id,
            }))
            .send()
            .await;

        let error = match response {
            Ok(response) if response.status().is_success() => return,
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                format!("ironpaw bridge returned {status}: {body}")
            }
            Err(err) => err.to_string(),
        };

        let _ = events_tx
            .send(AppEvent::Callback(CallbackPayload::Message {
                id: None,
                content: format!("error: {error}"),
                tool_calls: Vec::new(),
            }))
            .await;
    });
}

async fn start_callback_server(events_tx: mpsc::Sender<AppEvent>) -> io::Result<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let app = Router::new()
        .route("/", post(receive_callback))
        .with_state(events_tx);

    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            eprintln!("TUI callback server failed: {err}");
        }
    });

    Ok(format!("http://{addr}"))
}

async fn receive_callback(
    State(events_tx): State<mpsc::Sender<AppEvent>>,
    Json(payload): Json<CallbackPayload>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    events_tx
        .send(AppEvent::Callback(payload))
        .await
        .map_err(|_| {
            (
                axum::http::StatusCode::GONE,
                "TUI receiver closed".to_string(),
            )
        })?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

fn enter_terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    Ok(terminal)
}

fn leave_terminal(mut terminal: Terminal<CrosstermBackend<Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()
}

fn bridge_url(addr: &str) -> String {
    let base = if addr.starts_with("http://") || addr.starts_with("https://") {
        addr.trim_end_matches('/').to_string()
    } else {
        format!("http://{}", addr.trim_end_matches('/'))
    };
    format!("{base}/channel/local-client/message")
}

fn display_content(content: &str, tool_calls: &[serde_json::Value]) -> String {
    if !content.is_empty() {
        return content.to_string();
    }

    if !tool_calls.is_empty() {
        return format!("assistant requested {} tool call(s)", tool_calls.len());
    }

    "<empty assistant message>".to_string()
}

#[cfg(test)]
mod tests {
    use super::{bridge_url, display_content, scroll_offset};

    #[test]
    fn builds_bridge_url() {
        assert_eq!(
            bridge_url("127.0.0.1:7878"),
            "http://127.0.0.1:7878/channel/local-client/message"
        );
    }

    #[test]
    fn displays_assistant_message_content() {
        assert_eq!(display_content("hello back", &[]), "hello back");
    }

    #[test]
    fn displays_tool_call_summary_when_no_text() {
        let tool_calls = vec![serde_json::json!({"name": "send_message"})];
        assert_eq!(
            display_content("", &tool_calls),
            "assistant requested 1 tool call(s)"
        );
    }

    #[test]
    fn scroll_offset_starts_at_bottom_and_moves_up() {
        assert_eq!(scroll_offset(20, 5, 0), 15);
        assert_eq!(scroll_offset(20, 5, 3), 12);
    }

    #[test]
    fn scroll_offset_clamps_to_top() {
        assert_eq!(scroll_offset(20, 5, usize::MAX), 0);
        assert_eq!(scroll_offset(3, 5, 0), 0);
    }
}
