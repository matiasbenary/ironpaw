use super::common::*;

/// Verify the agent can route guest messages into a persistent file automation.
///
/// Run with:
///   cargo test --test live_tests -- --ignored --nocapture agent_stores_guest_messages
#[tokio::test]
#[ignore = "live: makes real NEAR AI calls, requires NEAR_API_KEY env var"]
async fn agent_stores_guest_messages() {
    let _ = dotenvy::dotenv();
    std::env::var("NEAR_API_KEY").expect("NEAR_API_KEY must be set");

    let files_dir =
        std::env::temp_dir().join(format!("ironpaw-live-test-files-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&files_dir).expect("create live test files directory");

    let bus = Bus::new(64);
    let (_task, history, automations, _, automation_files_dir) =
        start_ironpaw_config(&bus, files_dir);
    let existing = automations
        .list()
        .into_iter()
        .map(|automation| automation.name().to_string())
        .collect::<HashSet<_>>();
    let user_conversation = ConversationKey {
        channel: "tui".to_string(),
        chat_id: "guest-message-automation-owner".to_string(),
        thread_id: None,
    };

    publish_user_message(
        &bus,
        history.as_ref(),
        user_conversation,
        user_message("Store all guest messages in the guest.txt file."),
    );

    timeout(Duration::from_secs(120), async {
        loop {
            if automations.list().iter().any(|automation| {
                !existing.contains(automation.name())
                    && matches!(
                        automation,
                        Automation::OnGuestMessageIn {
                            conversation: None,
                            ..
                        }
                    )
            }) {
                return;
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("agent did not create an automation for all guest messages");

    let guest_conversation = ConversationKey {
        channel: "tui".to_string(),
        chat_id: "guest-message-room".to_string(),
        thread_id: None,
    };
    let first = "GUEST_MESSAGE_ALPHA_47";
    let second = "GUEST_MESSAGE_BETA_83";
    publish_guest_message(&bus, history.as_ref(), guest_conversation.clone(), first);
    publish_guest_message(&bus, history.as_ref(), guest_conversation, second);

    let content = timeout(Duration::from_secs(180), async {
        loop {
            if let Some(content) = find_guest_file(&automation_files_dir)
                && content.contains(first)
                && content.contains(second)
            {
                return content;
            }
            sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("guest.txt did not contain both guest messages");

    eprintln!("[live_test] guest.txt:\n{content}");
}

fn find_guest_file(root: &std::path::Path) -> Option<String> {
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(directory).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                directories.push(path);
            } else if path.file_name().is_some_and(|name| name == "guest.txt")
                && let Ok(content) = std::fs::read_to_string(path)
            {
                return Some(content);
            }
        }
    }
    None
}
