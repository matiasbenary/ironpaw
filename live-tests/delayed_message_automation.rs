use super::common::*;

/// Verify the agent can schedule a delayed message through an automation.
///
/// Run with:
///   cargo test --test live_tests -- --ignored --nocapture delayed_message_automation
#[tokio::test]
#[ignore = "live: makes real NEAR AI calls, requires NEAR_API_KEY env var"]
async fn delayed_message_automation() {
    let _ = dotenvy::dotenv();
    std::env::var("NEAR_API_KEY").expect("NEAR_API_KEY must be set");

    let files_dir = std::env::temp_dir().join("ironpaw-live-test-files");
    std::fs::create_dir_all(&files_dir).unwrap();

    let bus = Bus::new(32);
    let mut events = bus.subscribe();
    let (_task, history) = start_ironpaw(&bus, files_dir);

    let message = user_message(
        "In 3 seconds, send me a message that says exactly: BANANA. \
         The delayed message must contain only that single word.",
    );

    publish_user_message(
        &bus,
        history.as_ref(),
        ConversationKey {
            channel: "tui".to_string(),
            chat_id: "live-test-automation".to_string(),
            thread_id: None,
        },
        message,
    );

    let result = timeout(Duration::from_secs(60), async {
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
                eprintln!("[live_test] MessageOut: {content}");
                if content.trim() == "BANANA" {
                    return;
                }
            }
        }
    })
    .await;

    assert!(
        result.is_ok(),
        "timed out: the delayed BANANA message never arrived within 60 s"
    );
}
