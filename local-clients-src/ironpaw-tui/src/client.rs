use std::time::Duration;

use serde::Deserialize;
use tokio::sync::mpsc;

use crate::AppEvent;

#[derive(Deserialize)]
#[serde(tag = "kind")]
pub(crate) enum Event {
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
    #[serde(rename = "activity")]
    Activity { action: serde_json::Value },
    #[serde(rename = "choice")]
    Choice {
        question: String,
        options: Vec<UserInputOption>,
    },
    #[serde(rename = "error")]
    Error { message: String },
}

#[derive(Deserialize)]
pub(crate) struct UserInputOption {
    pub(crate) label: String,
    pub(crate) value: String,
}

pub(crate) fn url(addr: &str) -> String {
    let base = if addr.starts_with("http://") || addr.starts_with("https://") {
        addr.trim_end_matches('/').to_string()
    } else {
        format!("http://{}", addr.trim_end_matches('/'))
    };
    format!("{base}/client")
}

pub(crate) fn send(
    client: reqwest::Client,
    url: String,
    token: String,
    client_id: String,
    content: String,
    message_id: String,
    events_tx: mpsc::Sender<AppEvent>,
) {
    tokio::spawn(async move {
        let response = client
            .post(&url)
            .bearer_auth(&token)
            .json(&serde_json::json!({
                "client_id": client_id,
                "content": content,
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
            Err(error) => error.to_string(),
        };
        let _ = events_tx
            .send(AppEvent::Callback(Event::Error { message: error }))
            .await;
    });
}

pub(crate) fn subscribe(
    client: reqwest::Client,
    url: String,
    token: String,
    client_id: String,
    events_tx: mpsc::Sender<AppEvent>,
) {
    tokio::spawn(async move {
        loop {
            let response = client
                .get(&url)
                .bearer_auth(&token)
                .query(&[("client_id", &client_id)])
                .send()
                .await;
            match response {
                Ok(mut response) if response.status().is_success() => {
                    let mut buffer = String::new();
                    loop {
                        match response.chunk().await {
                            Ok(Some(chunk)) => {
                                buffer.push_str(&String::from_utf8_lossy(&chunk));
                                for payload in take_sse_payloads(&mut buffer) {
                                    if let Ok(event) = serde_json::from_str(&payload)
                                        && events_tx.send(AppEvent::Callback(event)).await.is_err()
                                    {
                                        return;
                                    }
                                }
                            }
                            Ok(None) | Err(_) => break,
                        }
                    }
                }
                Ok(response) => {
                    let message = format!("event stream returned {}", response.status());
                    let _ = events_tx
                        .send(AppEvent::Callback(Event::Error { message }))
                        .await;
                }
                Err(error) => {
                    let _ = events_tx
                        .send(AppEvent::Callback(Event::Error {
                            message: error.to_string(),
                        }))
                        .await;
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
}

fn take_sse_payloads(buffer: &mut String) -> Vec<String> {
    let mut payloads = Vec::new();
    while let Some(end) = buffer.find("\n\n") {
        let frame = buffer.drain(..end + 2).collect::<String>();
        if let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data: ")) {
            payloads.push(data.to_string());
        }
    }
    payloads
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_complete_sse_frames() {
        let mut buffer =
            "data: {\"kind\":\"error\",\"message\":\"one\"}\n\ndata: partial".to_string();
        let payloads = take_sse_payloads(&mut buffer);
        assert_eq!(payloads.len(), 1);
        assert_eq!(buffer, "data: partial");
    }
}
