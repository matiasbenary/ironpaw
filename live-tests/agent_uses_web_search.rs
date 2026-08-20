use super::common::*;

/// Verify the agent uses Brave-backed web_search for a current-events request.
///
/// Run with:
///   cargo test --test live_tests -- --ignored --nocapture --test-threads=1 agent_uses_web_search
#[tokio::test]
#[ignore = "live: makes real NEAR AI calls and requires BRAVE_SEARCH_API env var"]
async fn agent_uses_web_search() {
    let _ = dotenvy::dotenv();
    std::env::var("NEAR_API_KEY").expect("NEAR_API_KEY must be set");
    std::env::var("BRAVE_SEARCH_API").expect("BRAVE_SEARCH_API must be set");

    let files_dir = std::env::temp_dir().join("ironpaw-live-test-files");
    std::fs::create_dir_all(&files_dir).unwrap();

    let bus = Bus::new(32);
    let mut events = bus.subscribe();
    let (_task, history) = start_ironpaw(&bus, files_dir);
    let conversation = ConversationKey {
        channel: "tui".to_string(),
        chat_id: "live-test-web-search".to_string(),
        thread_id: None,
    };

    publish_user_message(
        &bus,
        history.as_ref(),
        conversation.clone(),
        user_message(
            "Search the web for the latest news about NEAR Protocol and summarize what you find. Use web_search.",
        ),
    );

    let result = timeout(Duration::from_secs(120), async {
        let mut saw_web_search = false;
        loop {
            match events.recv().await.expect("bus closed") {
                Event::ChannelAction {
                    conversation: event_conversation,
                    action: ironpaw::bus::ChannelAction::Progress { text },
                } if event_conversation == conversation => {
                    eprintln!("[live_test] ChannelAction::Progress: {text}");
                    if text == "calling web_search" {
                        saw_web_search = true;
                    }
                }
                Event::MessageOut {
                    channel,
                    chat_id: Some(chat_id),
                    message:
                        Message::Assistant {
                            text: Some(content),
                            ..
                        },
                    ..
                } if channel == conversation.channel && chat_id == conversation.chat_id => {
                    eprintln!("[live_test] MessageOut: {content}");
                    if saw_web_search && content.to_lowercase().contains("near") {
                        return;
                    }
                }
                _ => {}
            }
        }
    })
    .await;

    assert!(
        result.is_ok(),
        "timed out: agent did not use web_search and answer with current NEAR Protocol news"
    );
}
