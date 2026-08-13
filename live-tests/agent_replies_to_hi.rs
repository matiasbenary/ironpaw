use super::common::*;

/// Verify the agent can answer a minimal text message.
///
/// Run with:
///   cargo test --test live_tests -- --ignored --nocapture agent_replies_to_hi
#[tokio::test]
#[ignore = "live: makes real NEAR AI calls, requires NEAR_API_KEY env var"]
async fn agent_replies_to_hi() {
    let _ = dotenvy::dotenv();
    std::env::var("NEAR_API_KEY").expect("NEAR_API_KEY must be set");

    let files_dir = std::env::temp_dir().join("ironpaw-live-test-files");
    std::fs::create_dir_all(&files_dir).unwrap();

    let bus = Bus::new(32);
    let mut events = bus.subscribe();
    let (_task, history) = start_ironpaw(&bus, files_dir);

    let message = user_message("hi");

    publish_user_message(
        &bus,
        history.as_ref(),
        ConversationKey {
            channel: "tui".to_string(),
            chat_id: "live-test-hi".to_string(),
            thread_id: None,
        },
        message,
    );

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
