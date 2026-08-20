use super::common::*;

/// Verify the agent can schedule a message at an absolute wall-clock time,
/// which requires it to call the current_time tool.
///
/// Run with:
///   cargo test --test live_tests -- --ignored --nocapture scheduled_message_at_wall_clock_time
#[tokio::test]
#[ignore = "live: makes real NEAR AI calls, requires NEAR_API_KEY env var"]
async fn scheduled_message_at_wall_clock_time() {
    let _ = dotenvy::dotenv();
    std::env::var("NEAR_API_KEY").expect("NEAR_API_KEY must be set");

    let files_dir = std::env::temp_dir().join("ironpaw-live-test-files");
    std::fs::create_dir_all(&files_dir).unwrap();

    let bus = Bus::new(32);
    let mut events = bus.subscribe();
    let (_task, history) = start_ironpaw(&bus, files_dir);

    let target = chrono::Local::now() + chrono::Duration::seconds(8);
    let message = user_message(&format!(
        "Today at exactly {}, send me a message that says exactly: KIWI. \
         The scheduled message must contain only that single word.",
        target.format("%H:%M:%S"),
    ));

    publish_user_message(
        &bus,
        history.as_ref(),
        ConversationKey {
            channel: "tui".to_string(),
            chat_id: "live-test-at".to_string(),
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
                if content.trim() == "KIWI" {
                    return;
                }
            }
        }
    })
    .await;

    assert!(
        result.is_ok(),
        "timed out: the scheduled KIWI message never arrived within 60 s"
    );
}
