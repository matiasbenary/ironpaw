use super::common::*;

/// Verify the full dynamic-tool path: the agent authors a tool that fetches
/// the Hacker News top stories, schedules it, and the tool delivers them.
///
/// Run with:
///   cargo test --test live_tests -- --ignored --nocapture --test-threads=1 hacker_news_automation
#[tokio::test]
#[ignore = "live: makes real NEAR AI calls and fetches hacker-news.firebaseio.com"]
async fn hacker_news_automation() {
    let _ = dotenvy::dotenv();
    std::env::var("NEAR_API_KEY").expect("NEAR_API_KEY must be set");

    let files_dir = std::env::temp_dir().join("ironpaw-live-test-files");
    std::fs::create_dir_all(&files_dir).unwrap();

    let bus = Bus::new(32);
    let mut events = bus.subscribe();
    let (_task, history) = start_ironpaw(&bus, files_dir);

    let message = user_message(
        "Create a tool that fetches the titles of the current top 5 Hacker News \
         stories and sends them to me in a single message that starts with \
         'HN TOP 5'. Then schedule that tool to run once, 10 seconds from now.",
    );

    publish_user_message(
        &bus,
        history.as_ref(),
        ConversationKey {
            channel: "tui".to_string(),
            chat_id: "live-test-hn".to_string(),
            thread_id: None,
        },
        message,
    );

    let result = timeout(Duration::from_secs(120), async {
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
                if content.trim_start().starts_with("HN TOP 5") && content.len() > 40 {
                    return;
                }
            }
        }
    })
    .await;

    assert!(
        result.is_ok(),
        "timed out: the HN TOP 5 message never arrived within 120 s"
    );
}
