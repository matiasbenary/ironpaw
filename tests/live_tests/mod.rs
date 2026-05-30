use std::time::Duration;

use tokio::time::timeout;
use warden::{
    ai_engine::AiEngine,
    bus::{Bus, ChannelAddress, Event, Sender},
    openai::{EasyInputContent, EasyInputMessage, InputItem, Role},
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
            if let Event::MessageOut { channel, message, .. } = event {
                eprintln!("[live_test] MessageOut → channel={channel}, attachments={}", message.attachments.len());
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
