use super::common::*;

/// Verify the agent can check a product's current availability.
///
/// Run with:
///   cargo test --test live_tests -- --ignored --nocapture --test-threads=1 agent_checks_first_partner_stock
#[tokio::test]
#[ignore = "live: makes real NEAR AI calls and visits frikidenacimiento.es"]
async fn agent_checks_first_partner_stock() {
    let _ = dotenvy::dotenv();
    std::env::var("NEAR_API_KEY").expect("NEAR_API_KEY must be set");

    let files_dir = std::env::temp_dir().join("ironpaw-live-test-files");
    std::fs::create_dir_all(&files_dir).unwrap();

    let bus = Bus::new(32);
    let mut events = bus.subscribe();
    let (_task, history) = start_ironpaw(&bus, files_dir);
    let conversation = ConversationKey {
        channel: "tui".to_string(),
        chat_id: "live-test-first-partner-stock".to_string(),
        thread_id: None,
    };

    let reply = ask_and_wait_for_reply(
        &bus,
        &mut events,
        history.as_ref(),
        conversation.clone(),
        "can you check frikidenacimiento.es to see if they have pokemon first partner series 3 available to buy or reserve",
    )
    .await;
    eprintln!("[live_test] final reply: {reply}");

    let lower = reply.to_lowercase();
    assert!(
        lower.contains("first partner"),
        "reply did not identify the requested product: {reply}"
    );
    assert!(
        ["reserva", "reserve", "backorder", "pre-order"]
            .iter()
            .any(|term| lower.contains(term)),
        "reply did not report that Series 3 is currently available for reservation: {reply}"
    );
}
