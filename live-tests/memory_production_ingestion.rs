use super::common::*;

/// Verify a real conversation turn reaches background memory ingestion and
/// remains recallable after the active transcript is replaced by a generic
/// compaction summary.
///
/// Run with:
///   cargo test --test live_tests -- --ignored --nocapture --test-threads=1 memory_production_ingestion
#[tokio::test]
#[ignore = "live: exercises production memory ingestion using real NEAR AI calls"]
async fn memory_production_ingestion() {
    let _ = dotenvy::dotenv();
    std::env::var("NEAR_API_KEY").expect("NEAR_API_KEY must be set");
    let files_dir = std::env::temp_dir().join("ironpaw-live-test-files");
    std::fs::create_dir_all(&files_dir).expect("live test files directory");
    let bus = Bus::new(64);
    let mut events = bus.subscribe();
    let (_task, history, _, memory, _) = start_ironpaw_config(&bus, files_dir);
    let conversation = ConversationKey {
        channel: "tui".to_string(),
        chat_id: "memory-production-ingestion".to_string(),
        thread_id: None,
    };
    let question = "What is the code for my storage locker?";
    let required = vec!["q7-marble-42".to_string()];

    ask_and_wait_for_reply(
        &bus,
        &mut events,
        history.as_ref(),
        conversation.clone(),
        "My storage locker code is Q7-MARBLE-42. Please acknowledge that you noted it.",
    )
    .await;
    let entries = history
        .load_active(&HistoryId::Conversation(conversation.clone()))
        .expect("load conversation");
    history
        .save_conversation_summary(
            &HistoryId::Conversation(conversation.clone()),
            "Older conversation was compacted.",
            &entries.last().expect("conversation entry").metadata.id,
        )
        .expect("compact production ingestion conversation");
    assert_cold_history(history.as_ref(), &conversation);

    let memories = wait_for_memory_evidence(memory.as_ref(), question, &required).await;
    assert!(
        missing_required_terms(&memories, &required).is_empty(),
        "production-ingested memory lost required evidence after compaction: {memories:?}"
    );
}
