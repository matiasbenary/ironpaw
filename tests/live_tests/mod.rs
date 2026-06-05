use std::time::Duration;
use std::{path::PathBuf, sync::Arc};

use tokio::time::timeout;
use warden::{
    ai_engine::AiEngine,
    bus::{Bus, ChannelAddress, Event, Sender},
    history::SqliteHistory,
    message::{Attachment, Message},
    tools::{InternalTools, ai_inference::AiInferenceTool, send_message::SendMessageTool},
};

fn temp_history() -> Arc<SqliteHistory> {
    let path =
        std::env::temp_dir().join(format!("warden-test-history-{}.db", uuid::Uuid::new_v4()));
    Arc::new(SqliteHistory::new(path).expect("temp history"))
}

fn send_message_tool(bus: &Bus, files_dir: PathBuf) -> SendMessageTool {
    SendMessageTool::new(bus.clone(), move |file_id| {
        Attachment::from_file_id(&files_dir, file_id)
    })
}

/// Verify the agent can answer a minimal text message.
///
/// Run with:
///   cargo test --test live_tests -- --ignored --nocapture agent_replies_to_hi
#[tokio::test]
#[ignore = "live: makes real NEAR AI calls, requires NEAR_API_KEY env var"]
async fn agent_replies_to_hi() {
    let _ = dotenvy::dotenv();
    let near_api_key = std::env::var("NEAR_API_KEY").expect("NEAR_API_KEY must be set");

    let files_dir = std::env::temp_dir().join("warden-live-test-files");
    std::fs::create_dir_all(&files_dir).unwrap();

    let bus = Bus::new(32);
    let mut events = bus.subscribe();

    let tools = InternalTools::new(
        AiInferenceTool::new(near_api_key.clone()).unwrap(),
        send_message_tool(&bus, files_dir.clone()),
        near_api_key,
        files_dir,
        temp_history(),
    );

    let engine = AiEngine::new(bus.clone(), tools);
    let _task = engine.start();

    let message = user_message("hi");

    bus.publish(Event::MessageIn {
        channel: "tui".to_string(),
        sender: Sender::User,
        address: ChannelAddress {
            channel: "tui".to_string(),
            conversation_id: "live-test-hi".to_string(),
            thread_id: None,
        },
        message,
    });

    let reply = timeout(Duration::from_secs(30), async {
        loop {
            let event = events.recv().await.expect("bus closed");
            if let Event::MessageOut { message, .. } = event {
                let Message::Assistant {
                    text: Some(content),
                    ..
                } = message
                else {
                    continue;
                };
                {
                    eprintln!("[live_test] agent replied: {content}");
                    if !content.trim().is_empty() {
                        return content;
                    }
                }
            }
        }
    })
    .await
    .expect("timed out: agent did not reply to hi within 10 s");

    assert!(!reply.trim().is_empty());
}

/// Verify the LLM calls send_message with channel=telegram when asked from tui.
///
/// Run with:
///   cargo test --test live_tests -- --ignored --nocapture
#[tokio::test]
#[ignore = "live: makes real NEAR AI calls, requires NEAR_API_KEY env var"]
async fn send_message_routes_to_telegram() {
    let _ = dotenvy::dotenv();
    let near_api_key = std::env::var("NEAR_API_KEY").expect("NEAR_API_KEY must be set");

    let files_dir = std::env::temp_dir().join("warden-live-test-files");
    std::fs::create_dir_all(&files_dir).unwrap();

    let bus = Bus::new(32);
    let mut events = bus.subscribe();

    let tools = InternalTools::new(
        AiInferenceTool::new(near_api_key.clone()).unwrap(),
        send_message_tool(&bus, files_dir.clone()),
        near_api_key,
        files_dir.clone(),
        temp_history(),
    );

    let engine = AiEngine::new(bus.clone(), tools);
    let _task = engine.start();

    let address = ChannelAddress {
        channel: "tui".to_string(),
        conversation_id: "live-test-conversation".to_string(),
        thread_id: None,
    };

    let message = user_message("Please send me a message on telegram saying hello.");

    bus.publish(Event::MessageIn {
        channel: "tui".to_string(),
        sender: Sender::User,
        address,
        message,
    });

    let deadline = Duration::from_secs(30);
    let result = timeout(deadline, async {
        loop {
            let event = events.recv().await.expect("bus closed");
            if let Event::MessageOut { channel, .. } = event {
                eprintln!("[live_test] MessageOut → channel={channel}");
                if channel == "telegram" {
                    return;
                }
            }
        }
    })
    .await;

    assert!(
        result.is_ok(),
        "timed out: LLM did not send a message to telegram within 30 s"
    );
}

/// Verify the LLM routes an image to a different channel when asked.
///
/// Run with:
///   cargo test --test live_tests -- --ignored --nocapture
#[tokio::test]
#[ignore = "live: makes real NEAR AI calls, requires NEAR_API_KEY env var"]
async fn image_generation_routes_to_telegram() {
    let _ = dotenvy::dotenv();
    let near_api_key = std::env::var("NEAR_API_KEY").expect("NEAR_API_KEY must be set");

    // Temp dir with a stub image the mock generator will serve.
    let files_dir = std::env::temp_dir().join("warden-live-test-files");
    std::fs::create_dir_all(&files_dir).unwrap();
    std::fs::write(files_dir.join("1780086002.png"), b"stub-png").unwrap();

    let bus = Bus::new(32);
    let mut events = bus.subscribe();

    let tools = InternalTools::new(
        AiInferenceTool::new(near_api_key.clone()).unwrap(),
        send_message_tool(&bus, files_dir.clone()),
        near_api_key,
        files_dir.clone(),
        temp_history(),
    );

    let engine = AiEngine::new(bus.clone(), tools);
    let _task = engine.start();

    let address = ChannelAddress {
        channel: "tui".to_string(),
        conversation_id: "live-test-conversation".to_string(),
        thread_id: None,
    };

    let message = user_message("Create an image of a dog and send it to me in telegram.");

    bus.publish(Event::MessageIn {
        channel: "tui".to_string(),
        sender: Sender::User,
        address,
        message,
    });

    // Collect MessageOut events for up to 30 s, looking for a telegram delivery with attachment.
    let deadline = Duration::from_secs(30);
    let result = timeout(deadline, async {
        loop {
            let event = events.recv().await.expect("bus closed");
            if let Event::MessageOut {
                channel, message, ..
            } = event
            {
                let Message::Assistant { attachments, .. } = message else {
                    continue;
                };
                eprintln!(
                    "[live_test] MessageOut → channel={channel}, attachments={}",
                    attachments.len()
                );
                if channel == "telegram" && !attachments.is_empty() {
                    return;
                }
            }
        }
    })
    .await;

    assert!(
        result.is_ok(),
        "timed out: LLM did not send an image to telegram within 30 s"
    );
}

/// Verify the LLM can read the contents of an attached PDF.
///
/// Run with:
///   cargo test --test live_tests -- --ignored --nocapture
#[tokio::test]
#[ignore = "live: makes real NEAR AI calls, requires NEAR_API_KEY env var"]
async fn agent_reads_pdf_content() {
    let _ = dotenvy::dotenv();
    let near_api_key = std::env::var("NEAR_API_KEY").expect("NEAR_API_KEY must be set");

    let files_dir = std::env::temp_dir().join("warden-live-test-files");
    std::fs::create_dir_all(&files_dir).unwrap();

    let bus = Bus::new(32);
    let mut events = bus.subscribe();

    let tools = InternalTools::new(
        AiInferenceTool::new(near_api_key.clone()).unwrap(),
        send_message_tool(&bus, files_dir.clone()),
        near_api_key,
        files_dir.clone(),
        temp_history(),
    );

    let engine = AiEngine::new(bus.clone(), tools);
    let _task = engine.start();

    let address = ChannelAddress {
        channel: "tui".to_string(),
        conversation_id: "live-test-pdf".to_string(),
        thread_id: None,
    };

    // Mimic what the bridge does: store the file to disk, then tell the model about it.
    let pdf_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/live_tests/files/test.pdf"
    );
    let pdf_bytes = std::fs::read(pdf_path).expect("test PDF should be readable");
    let file_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as u32;
    let source = files_dir.join(format!("{file_id}.pdf"));
    std::fs::write(&source, &pdf_bytes).unwrap();

    let message = Message::User {
        text: "Call the inspect_file tool to read the attached PDF, then tell me what text it contains."
            .to_string(),
        attachments: vec![Attachment {
            file_id,
            mime_type: "application/pdf".to_string(),
            filename: Some("test.pdf".to_string()),
            source,
        }],
    };

    bus.publish(Event::MessageIn {
        channel: "tui".to_string(),
        sender: Sender::User,
        address,
        message,
    });

    let deadline = Duration::from_secs(10);
    let result = timeout(deadline, async {
        loop {
            let event = events.recv().await.expect("bus closed");
            if let Event::MessageOut { message, .. } = event {
                let Message::Assistant {
                    text: Some(content),
                    ..
                } = &message
                else {
                    continue;
                };
                {
                    eprintln!("[live_test] agent replied: {content}");
                    if content.to_lowercase().contains("blue") {
                        return;
                    }
                }
            }
        }
    })
    .await;

    assert!(
        result.is_ok(),
        "timed out: agent did not reply with the PDF content within 30 s"
    );
}

/// Verify the full agent loop can reply after receiving an image input.
///
/// Run with:
///   cargo test --test live_tests -- --ignored --nocapture agent_identifies_dog_image
#[tokio::test]
#[ignore = "live: makes real NEAR AI calls, requires NEAR_API_KEY env var"]
async fn agent_identifies_dog_image() {
    let _ = dotenvy::dotenv();
    let near_api_key = std::env::var("NEAR_API_KEY").expect("NEAR_API_KEY must be set");

    let files_dir = std::env::temp_dir().join("warden-live-test-files");
    std::fs::create_dir_all(&files_dir).unwrap();

    let bus = Bus::new(32);
    let mut events = bus.subscribe();

    let tools = InternalTools::new(
        AiInferenceTool::new(near_api_key.clone()).unwrap(),
        send_message_tool(&bus, files_dir.clone()),
        near_api_key,
        files_dir.clone(),
        temp_history(),
    );

    let engine = AiEngine::new(bus.clone(), tools);
    let _task = engine.start();

    let image_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/live_tests/files/test.jpg"
    );
    let file_id = 123456789u32;
    let source = files_dir.join(format!("{file_id}.jpg"));
    std::fs::write(
        &source,
        std::fs::read(image_path).expect("test image should be readable"),
    )
    .unwrap();
    let message = Message::User {
        text: "Use inspect_file with query 'What animal is in this image? Answer with one word.'"
            .to_string(),
        attachments: vec![Attachment {
            file_id,
            mime_type: "image/jpeg".to_string(),
            filename: Some("test.jpg".to_string()),
            source,
        }],
    };

    bus.publish(Event::MessageIn {
        channel: "tui".to_string(),
        sender: Sender::User,
        address: ChannelAddress {
            channel: "tui".to_string(),
            conversation_id: "live-test-image".to_string(),
            thread_id: None,
        },
        message,
    });

    let reply = timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.expect("bus closed");
            if let Event::MessageOut { message, .. } = event {
                let Message::Assistant {
                    text: Some(content),
                    ..
                } = message
                else {
                    continue;
                };
                {
                    eprintln!("[live_test] agent replied: {content}");
                    return content;
                }
            }
        }
    })
    .await
    .expect("timed out: agent did not reply about the test image within 30 s");

    assert!(
        reply.to_lowercase().contains("dog"),
        "agent did not identify the test image as a dog; reply was: {reply}"
    );
}

fn user_message(text: &str) -> Message {
    Message::User {
        text: text.to_string(),
        attachments: Vec::new(),
    }
}
