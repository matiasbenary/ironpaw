use super::common::*;

/// Verify the agent can use Brave-backed web_search for current web lookup.
///
/// Run with:
///   cargo test --test live_tests -- --ignored --nocapture --test-threads=1 agent_uses_web_search_for_wikipedia
#[tokio::test]
#[ignore = "live: makes real NEAR AI calls and requires BRAVE_SEARCH_API env var"]
async fn agent_uses_web_search_for_wikipedia() {
    let _ = dotenvy::dotenv();
    std::env::var("NEAR_API_KEY").expect("NEAR_API_KEY must be set");
    std::env::var("BRAVE_SEARCH_API").expect("BRAVE_SEARCH_API must be set");

    let files_dir = std::env::temp_dir().join("ironpaw-live-test-files");
    std::fs::create_dir_all(&files_dir).unwrap();

    let bus = Bus::new(32);
    let mut events = bus.subscribe();
    let (_task, history) = start_ironpaw(&bus, files_dir);

    let message = user_message(
        "Use web_search to search the web for Illia Polosukhin Wikipedia. \
         Then answer me with one sentence that includes the words \
         Illia Polosukhin and NEAR.",
    );

    publish_user_message(
        &bus,
        history.as_ref(),
        ConversationKey {
            channel: "tui".to_string(),
            chat_id: "live-test-web-search".to_string(),
            thread_id: None,
        },
        message,
    );

    let result = timeout(Duration::from_secs(60), async {
        let mut saw_web_search = false;
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
                if content.contains("calling web_search") {
                    saw_web_search = true;
                    continue;
                }
                let lower = content.to_lowercase();
                if saw_web_search && lower.contains("illia polosukhin") && lower.contains("near") {
                    return;
                }
            }
        }
    })
    .await;

    assert!(
        result.is_ok(),
        "timed out: agent did not use web_search and answer about Illia Polosukhin within 60 s"
    );
}
