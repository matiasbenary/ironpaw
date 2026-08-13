use super::common::*;

/// Verify the full agent loop can reply after receiving an image input.
///
/// Run with:
///   cargo test --test live_tests -- --ignored --nocapture agent_identifies_dog_image
#[tokio::test]
#[ignore = "live: makes real NEAR AI calls, requires NEAR_API_KEY env var"]
async fn agent_identifies_dog_image() {
    let _ = dotenvy::dotenv();
    std::env::var("NEAR_API_KEY").expect("NEAR_API_KEY must be set");

    let files_dir = std::env::temp_dir().join("ironpaw-live-test-files");
    std::fs::create_dir_all(&files_dir).unwrap();

    let bus = Bus::new(32);
    let mut events = bus.subscribe();
    let (_task, history) = start_ironpaw(&bus, files_dir.clone());

    let image_path = concat!(env!("CARGO_MANIFEST_DIR"), "/live-tests/files/test.jpg");
    let filename = "test.jpg".to_string();
    let source = files_dir.join(&filename);
    std::fs::write(
        &source,
        std::fs::read(image_path).expect("test image should be readable"),
    )
    .unwrap();
    let message = Message::User {
        text: "Use inspect_file with query 'What animal is in this image? Answer with one word.'"
            .to_string(),
        attachments: vec![Attachment {
            filename,
            mime_type: "image/jpeg".to_string(),
            source,
        }],
    };

    publish_user_message(
        &bus,
        history.as_ref(),
        ConversationKey {
            channel: "tui".to_string(),
            chat_id: "live-test-image".to_string(),
            thread_id: None,
        },
        message,
    );

    timeout(Duration::from_secs(60), async {
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
                    if content.to_lowercase().contains("dog") {
                        return;
                    }
                }
            }
        }
    })
    .await
    .expect("timed out: agent did not reply about the test image within 30 s");
}
