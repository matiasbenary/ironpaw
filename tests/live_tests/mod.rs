use std::time::Duration;

use tokio::time::timeout;
use warden::{
    ai_engine::AiEngine,
    bus::{Bus, ChannelAddress, Event, Sender},
    openai::{
        EasyInputContent, EasyInputMessage, ImageDetail, InputContent, InputFileContent,
        InputImageContent, InputItem, InputMessage, InputRole, InputTextContent, Role,
    },
    tools::{InternalTools, ai_inference::AiInferenceTool, send_message::SendMessageTool},
};

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
        SendMessageTool::new(bus.clone(), files_dir.clone()),
        near_api_key,
        files_dir,
    );

    let engine = AiEngine::new(bus.clone(), tools);
    let _task = engine.start();

    let address = ChannelAddress {
        channel: "tui".to_string(),
        conversation_id: "live-test-conversation".to_string(),
        thread_id: None,
    };

    let message: InputItem = EasyInputMessage {
        r#type: Default::default(),
        role: Role::User,
        content: EasyInputContent::Text(
            "Create an image of a dog and send it to me in telegram.".to_string(),
        ),
        phase: None,
    }
    .into();

    bus.publish(Event::MessageIn {
        channel: "tui".to_string(),
        sender: Sender::User,
        address,
        message,
    });

    // Collect MessageOut events for up to 15 s, looking for a telegram delivery with attachment.
    let deadline = Duration::from_secs(15);
    let result = timeout(deadline, async {
        loop {
            let event = events.recv().await.expect("bus closed");
            if let Event::MessageOut {
                channel, message, ..
            } = event
            {
                eprintln!(
                    "[live_test] MessageOut → channel={channel}, attachments={}",
                    message.attachments.len()
                );
                if channel == "telegram" && !message.attachments.is_empty() {
                    return;
                }
            }
        }
    })
    .await;

    assert!(
        result.is_ok(),
        "timed out: LLM did not send an image to telegram within 15 s"
    );
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
        SendMessageTool::new(bus.clone(), files_dir.clone()),
        near_api_key,
        files_dir,
    );

    let engine = AiEngine::new(bus.clone(), tools);
    let _task = engine.start();

    let address = ChannelAddress {
        channel: "tui".to_string(),
        conversation_id: "live-test-conversation".to_string(),
        thread_id: None,
    };

    let message: InputItem = EasyInputMessage {
        r#type: Default::default(),
        role: Role::User,
        content: EasyInputContent::Text(
            "Please send me a message on telegram saying hello.".to_string(),
        ),
        phase: None,
    }
    .into();

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
        SendMessageTool::new(bus.clone(), files_dir.clone()),
        near_api_key,
        files_dir,
    );

    let engine = AiEngine::new(bus.clone(), tools);
    let _task = engine.start();

    let image_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/live_tests/files/test.jpg"
    );
    let image_base64 = image_file_base64(image_path);
    let message: InputItem = InputMessage {
        role: InputRole::User,
        content: vec![
            InputContent::InputText(InputTextContent {
                text: "What animal is in this image? Answer with one word.".to_string(),
            }),
            InputContent::InputImage(InputImageContent {
                detail: ImageDetail::Auto,
                file_id: None,
                image_url: Some(format!("data:image/jpeg;base64,{image_base64}")),
            }),
        ],
        status: None,
    }
    .into();

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

    let reply = timeout(Duration::from_secs(30), async {
        loop {
            let event = events.recv().await.expect("bus closed");
            if let Event::MessageOut { message, .. } = event {
                if let Some(content) = message.content {
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

/// Build a minimal valid single-page PDF containing the given text.
/// Returns the bytes as a base64-encoded string.
fn minimal_pdf_base64(text: &str) -> String {
    // A hand-crafted minimal PDF. Object offsets are computed for the fixed
    // structure below; the only variable part is the text stream.
    let stream = format!("BT /F1 12 Tf 100 700 Td ({text}) Tj ET");
    let stream_len = stream.len();

    let pdf = format!(
        "%PDF-1.4\n\
         1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
         2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n\
         3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792]\n\
         /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>\nendobj\n\
         4 0 obj\n<< /Length {stream_len} >>\nstream\n{stream}\nendstream\nendobj\n\
         5 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\nendobj\n\
         xref\n0 6\n0000000000 65535 f \n\
         trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n0\n%%EOF\n"
    );

    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(pdf.as_bytes())
}

fn image_file_base64(path: &str) -> String {
    let bytes = std::fs::read(path).expect("test image should be readable");

    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}
