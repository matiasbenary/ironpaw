use super::common::*;

/// Verify the LLM routes an image to a different channel when asked.
///
/// Run with:
///   cargo test --test live_tests -- --ignored --nocapture
#[tokio::test]
#[ignore = "live: makes real NEAR AI calls, requires NEAR_API_KEY env var"]
async fn image_generation_routes_to_telegram() {
    let _ = dotenvy::dotenv();
    std::env::var("NEAR_API_KEY").expect("NEAR_API_KEY must be set");

    // Temp dir with a stub image the mock generator will serve.
    let files_dir = std::env::temp_dir().join("ironpaw-live-test-files");
    std::fs::create_dir_all(&files_dir).unwrap();
    std::fs::write(files_dir.join("1780086002.png"), b"stub-png").unwrap();

    let bus = Bus::new(32);
    let mut events = bus.subscribe();
    let (_task, history) = start_ironpaw(&bus, files_dir);

    let conversation = ConversationKey {
        channel: "tui".to_string(),
        chat_id: "live-test-conversation".to_string(),
        thread_id: None,
    };

    let message = user_message(
        "Create an image of a dog and send it to my Telegram chat ID live-test-telegram.",
    );

    publish_user_message(&bus, history.as_ref(), conversation, message);

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
