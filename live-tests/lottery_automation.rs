use super::common::*;

/// Verify the agent can create a recurring task that remembers prior runs and deletes itself.
///
/// Run with:
///   cargo test --test live_tests -- --ignored --nocapture --test-threads=1 lottery_automation
#[tokio::test]
#[ignore = "live: makes real NEAR AI calls, requires NEAR_API_KEY env var"]
async fn lottery_automation() {
    let _ = dotenvy::dotenv();
    std::env::var("NEAR_API_KEY").expect("NEAR_API_KEY must be set");

    let files_dir = std::env::temp_dir().join("ironpaw-live-test-files");
    std::fs::create_dir_all(&files_dir).unwrap();

    let bus = Bus::new(32);
    let (_task, history, automations) = start_ironpaw_with_automations(&bus, files_dir);
    let existing = automations
        .list()
        .into_iter()
        .map(|automation| automation.name)
        .collect::<HashSet<_>>();
    let conversation = ConversationKey {
        channel: "tui".to_string(),
        chat_id: "live-lottery".to_string(),
        thread_id: None,
    };
    publish_user_message(
        &bus,
        history.as_ref(),
        conversation,
        user_message(
            "I want to play the lotery, send me a lucky number every 5 seconds, do not send me the same number twice, stop after 3 numbers have been sent",
        ),
    );

    let automation = timeout(Duration::from_secs(60), async {
        loop {
            if let Some(automation) = automations.list().into_iter().find(|automation| {
                !existing.contains(&automation.name)
                    && automation.effect["type"] == "agent_task"
                    && automation.effect["tools"].as_array().is_some_and(|tools| {
                        ["send_message", "delete_automation"]
                            .into_iter()
                            .all(|name| tools.iter().any(|tool| tool == name))
                    })
            }) {
                break automation;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("agent did not create the lottery automation");
    assert_eq!(automation.trigger["type"], "cron");
    let cron = automation.trigger["expression"]
        .as_str()
        .expect("cron expression");
    assert_eq!(cron.split_whitespace().next(), Some("*/5"));
    assert!(cron.split_whitespace().count() >= 6);
    let automation_name = automation.name;

    timeout(Duration::from_secs(120), async {
        while automations.contains(&automation_name) {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("automation did not delete itself");

    fn lucky_number(content: &str) -> Option<u32> {
        let marker = "lucky number";
        let lower = content.to_ascii_lowercase();
        let suffix = &content[lower.find(marker)? + marker.len()..];
        let digits = suffix
            .chars()
            .skip_while(|character| !character.is_ascii_digit())
            .take_while(|character| character.is_ascii_digit())
            .collect::<String>();
        let number = digits.parse().ok()?;
        (1..=99).contains(&number).then_some(number)
    }

    let entries = history
        .load_scope(&HistoryScope::Automation(automation_name))
        .expect("load automation history");
    let numbers = entries
        .iter()
        .filter_map(|entry| {
            let Message::Assistant { tool_calls, .. } = &entry.message else {
                return None;
            };
            tool_calls.iter().find_map(|call| {
                (call.name == "send_message")
                    .then(|| serde_json::from_str::<serde_json::Value>(&call.arguments).ok())
                    .flatten()
                    .and_then(|args| args["content"].as_str().and_then(lucky_number))
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(numbers.len(), 3);
    assert_eq!(numbers.iter().collect::<HashSet<_>>().len(), 3);
    let invocations = entries
        .iter()
        .filter(|entry| matches!(entry.message, Message::User { .. }))
        .count();
    assert_eq!(invocations, 3);
}
