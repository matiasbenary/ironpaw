use super::common::*;

/// Run the cold-history scenarios with Ironpaw's native automatic memory recall.
///
/// Run with:
///   cargo test --test live_tests -- --ignored --nocapture --test-threads=1 memory_native_eval
#[tokio::test]
#[ignore = "live: evaluates native memory using real NEAR AI calls"]
async fn memory_native_eval() {
    let _ = dotenvy::dotenv();
    std::env::var("NEAR_API_KEY").expect("NEAR_API_KEY must be set");
    let fixture = memory_eval_fixture();
    let probe_filter = std::env::var("IRONPAW_MEMORY_EVAL_PROBE").ok();
    let files_dir = std::env::temp_dir().join("ironpaw-live-test-files");
    std::fs::create_dir_all(&files_dir).unwrap();
    let bus = Bus::new(64);
    let mut events = bus.subscribe();
    let (_task, history, _, memory) = start_ironpaw_config(&bus, files_dir);
    let mut passed = 0;
    let mut evaluated = 0;
    let mut retrieval_failures = Vec::new();
    let mut excessive_search_cases = Vec::new();
    ingest_fixture(&memory, &fixture.messages).await;

    for case in &fixture.probes {
        if probe_filter
            .as_deref()
            .is_some_and(|filter| filter != case.name)
        {
            continue;
        }
        evaluated += 1;
        let conversation = ConversationKey {
            channel: "tui".to_string(),
            chat_id: format!("memory-{}", case.name),
            thread_id: None,
        };
        seed_cold_history(history.as_ref(), &conversation, &fixture.messages);
        let started = std::time::Instant::now();
        let memories = memory
            .recall(&case.question)
            .await
            .expect("recall native memory");
        let missing = missing_required_terms(&memories, &case.required);
        if !missing.is_empty() {
            retrieval_failures.push(format!("{} missing {missing:?}", case.name));
        }
        let reply = ask_and_wait_for_reply(
            &bus,
            &mut events,
            history.as_ref(),
            conversation.clone(),
            &case.question,
        )
        .await;
        let lower = reply.to_lowercase();
        let correct = case.required.iter().all(|term| lower.contains(term))
            && case.forbidden.iter().all(|term| !lower.contains(term));
        let search_count = search_history_call_count(
            &history
                .load(&conversation)
                .expect("load memory evaluation transcript"),
        );
        if search_count > 1 {
            excessive_search_cases.push(format!("{} ({search_count} calls)", case.name));
        }
        passed += usize::from(correct);
        eprintln!(
            "[memory_eval] case={} correct={} search_history_calls={} memories={:?} latency_ms={} reply={:?}",
            case.name,
            correct,
            search_count,
            memories,
            started.elapsed().as_millis(),
            reply,
        );
    }

    eprintln!("[memory_eval] score={passed}/{}", evaluated);
    assert!(evaluated > 0, "memory probe filter matched no cases");
    if probe_filter.is_none() {
        assert_eq!(
            evaluated, 10,
            "full memory evaluation must run all 10 probes"
        );
    }
    assert!(
        retrieval_failures.is_empty(),
        "native recall omitted required evidence: {}",
        retrieval_failures.join(", ")
    );
    assert!(
        excessive_search_cases.is_empty(),
        "search_history was invoked more than once: {}",
        excessive_search_cases.join(", ")
    );
    let required_score = if probe_filter.is_some() { evaluated } else { 9 };
    assert!(
        passed >= required_score,
        "memory answer score {passed}/{evaluated} is below required {required_score}/{evaluated}"
    );
}
