use std::time::Duration;
use std::{path::PathBuf, sync::Arc};

use tokio::time::timeout;
use warden::{
    agent::Agent,
    automations::{Automations, SqliteAutomations},
    bus::{Bus, ConversationKey, Event, Sender},
    history::{History, HistoryEntry, MessageMetadata, SqliteHistory},
    message::{Attachment, Message},
    tools::{
        InternalTools, ToolHandler,
        messages::{DeleteMessageTool, SendMessageTool},
        raw_inference::RawInferenceTool,
        setup::{OnboardingTool, SetupServices},
        wasm::{self, WasmTool},
    },
};

fn start_warden(bus: &Bus, files_dir: PathBuf) -> (tokio::task::JoinHandle<()>, Arc<dyn History>) {
    let history = temp_history();
    let layout = warden::config::RuntimeLayout::from_root(
        std::env::temp_dir().join(format!("warden-live-test-runtime-{}", uuid::Uuid::new_v4())),
    );
    let secrets = warden::secrets::Secrets::for_tests();
    if let Ok(near_api_key) = std::env::var("NEAR_API_KEY") {
        secrets.set("NEAR_API_KEY", &near_api_key).unwrap();
    }
    if let Ok(brave_search_api) = std::env::var("BRAVE_SEARCH_API") {
        secrets.set("BRAVE_SEARCH_API", &brave_search_api).unwrap();
    }
    let tools = InternalTools::new(
        bus.clone(),
        layout.clone(),
        RawInferenceTool::new(secrets.tools.clone()).unwrap(),
        send_message_tool(bus, files_dir.clone()),
        DeleteMessageTool::new(bus.clone()),
        files_dir,
        Arc::clone(&history),
        secrets.clone(),
    );
    let tools = Arc::new(tools);
    let tool_handler = Arc::new(ToolHandler::new(
        bus.clone(),
        Arc::clone(&tools.registry),
        Arc::clone(&history),
    ));
    let agent = Arc::new(Agent::new(
        bus.clone(),
        Arc::clone(&tools.registry),
        tool_handler,
        Arc::clone(&history),
        tools.raw_inference.clone(),
    ));
    let automations_db = std::env::temp_dir().join(format!(
        "warden-live-test-automations-{}.db",
        uuid::Uuid::new_v4()
    ));
    let automation_store =
        Arc::new(SqliteAutomations::new(automations_db).expect("temp automations db"));
    let automations = Arc::new(Automations::new(
        bus.clone(),
        Arc::clone(&tools),
        Arc::clone(&agent),
        automation_store,
    ));
    install_web_search_for_live_tests(&layout, &tools, &secrets);
    let setup_services = Arc::new(SetupServices::new(
        bus.clone(),
        Arc::clone(&tools),
        Arc::clone(&automations),
        layout,
        secrets,
    ));
    tools
        .registry
        .add(Arc::new(OnboardingTool::new(setup_services)))
        .expect("onboarding registers");
    automations
        .add(Automations::handle_conversation())
        .expect("stock automation registers");
    automations
        .add_transient(Automations::handle_tool_result())
        .expect("tool result automation registers");
    (automations.start(), history)
}

fn install_web_search_for_live_tests(
    layout: &warden::config::RuntimeLayout,
    tools: &Arc<InternalTools>,
    secrets: &warden::secrets::Secrets,
) {
    if !secrets.exists("BRAVE_SEARCH_API") {
        return;
    }

    let tools_dir = layout.tools_dir();
    std::fs::create_dir_all(&tools_dir).expect("live test tools dir");
    let package_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tools-src/web-search");
    std::fs::copy(
        package_dir.join("web-search.wasm"),
        tools_dir.join("web_search.wasm"),
    )
    .expect("copy web_search wasm");
    std::fs::copy(
        package_dir.join("web-search.config.example.json"),
        tools_dir.join("web_search.config.json"),
    )
    .expect("copy web_search config");

    let manifest = wasm::discover(layout)
        .expect("discover wasm tools")
        .into_iter()
        .find(|manifest| manifest.name == "web_search")
        .expect("web_search manifest");
    let tool =
        WasmTool::from_manifest(manifest, secrets.tools.clone()).expect("load web_search tool");
    tools
        .registry
        .add(Arc::new(tool))
        .expect("register web_search tool");
}

fn temp_history() -> Arc<dyn History> {
    let path =
        std::env::temp_dir().join(format!("warden-test-history-{}.db", uuid::Uuid::new_v4()));
    Arc::new(SqliteHistory::new(path).expect("temp history"))
}

fn publish_user_message(
    bus: &Bus,
    history: &dyn History,
    conversation: ConversationKey,
    message: Message,
) {
    history.append(
        &conversation,
        &[HistoryEntry {
            message: message.clone(),
            metadata: MessageMetadata::new(conversation.clone(), None),
        }],
    );
    bus.publish(Event::MessageIn {
        channel: conversation.channel.clone(),
        sender: Sender::User,
        channel_user_id: None,
        conversation,
        message_id: None,
        message,
    });
}

fn send_message_tool(bus: &Bus, files_dir: PathBuf) -> SendMessageTool {
    SendMessageTool::new(bus.clone(), move |file_id| {
        Attachment::from_file_id(&files_dir, file_id)
    })
}

/// Verify the agent can answer a minimal text message.
///
/// Run with:
///   cargo test --test live_tests -- --ignored --nocapture agent_replies_to_hi
#[tokio::test]
#[ignore = "live: makes real NEAR AI calls, requires NEAR_API_KEY env var"]
async fn agent_replies_to_hi() {
    let _ = dotenvy::dotenv();
    std::env::var("NEAR_API_KEY").expect("NEAR_API_KEY must be set");

    let files_dir = std::env::temp_dir().join("warden-live-test-files");
    std::fs::create_dir_all(&files_dir).unwrap();

    let bus = Bus::new(32);
    let mut events = bus.subscribe();
    let (_task, history) = start_warden(&bus, files_dir);

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

/// Verify the LLM calls send_message with channel=telegram when asked from tui.
///
/// Run with:
///   cargo test --test live_tests -- --ignored --nocapture
#[tokio::test]
#[ignore = "live: makes real NEAR AI calls, requires NEAR_API_KEY env var"]
async fn send_message_routes_to_telegram() {
    let _ = dotenvy::dotenv();
    std::env::var("NEAR_API_KEY").expect("NEAR_API_KEY must be set");

    let files_dir = std::env::temp_dir().join("warden-live-test-files");
    std::fs::create_dir_all(&files_dir).unwrap();

    let bus = Bus::new(32);
    let mut events = bus.subscribe();
    let (_task, history) = start_warden(&bus, files_dir);

    let conversation = ConversationKey {
        channel: "tui".to_string(),
        chat_id: "live-test-conversation".to_string(),
        thread_id: None,
    };

    let message = user_message("Please send me a message on telegram saying hello.");

    publish_user_message(&bus, history.as_ref(), conversation, message);

    let deadline = Duration::from_secs(30);
    let result = timeout(deadline, async {
        loop {
            let event = events.recv().await.expect("bus closed");
            if let Event::MessageOut { channel, .. } = event {
                eprintln!("[live_test] MessageOut → channel={channel}");
                if channel == "telegram" {
                    return;
                }
            }
        }
    })
    .await;

    assert!(
        result.is_ok(),
        "timed out: LLM did not send a message to telegram within 30 s"
    );
}

/// Verify the LLM routes an image to a different channel when asked.
///
/// Run with:
///   cargo test --test live_tests -- --ignored --nocapture
#[tokio::test]
#[ignore = "live: makes real NEAR AI calls, requires NEAR_API_KEY env var"]
async fn image_generation_routes_to_telegram() {
    let _ = dotenvy::dotenv();
    std::env::var("NEAR_API_KEY").expect("NEAR_API_KEY must be set");

    // Temp dir with a stub image the mock generator will serve.
    let files_dir = std::env::temp_dir().join("warden-live-test-files");
    std::fs::create_dir_all(&files_dir).unwrap();
    std::fs::write(files_dir.join("1780086002.png"), b"stub-png").unwrap();

    let bus = Bus::new(32);
    let mut events = bus.subscribe();
    let (_task, history) = start_warden(&bus, files_dir);

    let conversation = ConversationKey {
        channel: "tui".to_string(),
        chat_id: "live-test-conversation".to_string(),
        thread_id: None,
    };

    let message = user_message("Create an image of a dog and send it to me in telegram.");

    publish_user_message(&bus, history.as_ref(), conversation, message);

    // Collect MessageOut events for up to 30 s, looking for a telegram delivery with attachment.
    let deadline = Duration::from_secs(30);
    let result = timeout(deadline, async {
        loop {
            let event = events.recv().await.expect("bus closed");
            if let Event::MessageOut {
                channel, message, ..
            } = event
            {
                let Message::Assistant { attachments, .. } = message else {
                    continue;
                };
                eprintln!(
                    "[live_test] MessageOut → channel={channel}, attachments={}",
                    attachments.len()
                );
                if channel == "telegram" && !attachments.is_empty() {
                    return;
                }
            }
        }
    })
    .await;

    assert!(
        result.is_ok(),
        "timed out: LLM did not send an image to telegram within 30 s"
    );
}

/// Verify the LLM can read the contents of an attached PDF.
///
/// Run with:
///   cargo test --test live_tests -- --ignored --nocapture
#[tokio::test]
#[ignore = "live: makes real NEAR AI calls, requires NEAR_API_KEY env var"]
async fn agent_reads_pdf_content() {
    let _ = dotenvy::dotenv();
    std::env::var("NEAR_API_KEY").expect("NEAR_API_KEY must be set");

    let files_dir = std::env::temp_dir().join("warden-live-test-files");
    std::fs::create_dir_all(&files_dir).unwrap();

    let bus = Bus::new(32);
    let mut events = bus.subscribe();
    let (_task, history) = start_warden(&bus, files_dir.clone());

    let conversation = ConversationKey {
        channel: "tui".to_string(),
        chat_id: "live-test-pdf".to_string(),
        thread_id: None,
    };

    // Mimic what the bridge does: store the file to disk, then tell the model about it.
    let pdf_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/live_tests/files/test.pdf"
    );
    let pdf_bytes = std::fs::read(pdf_path).expect("test PDF should be readable");
    let file_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as u32;
    let source = files_dir.join(format!("{file_id}.pdf"));
    std::fs::write(&source, &pdf_bytes).unwrap();

    let message = Message::User {
        text: "Call the inspect_file tool to read the attached PDF, then tell me what text it contains."
            .to_string(),
        attachments: vec![Attachment {
            file_id,
            mime_type: "application/pdf".to_string(),
            filename: Some("test.pdf".to_string()),
            source,
        }],
    };

    publish_user_message(&bus, history.as_ref(), conversation, message);

    let deadline = Duration::from_secs(10);
    let result = timeout(deadline, async {
        loop {
            let event = events.recv().await.expect("bus closed");
            if let Event::MessageOut { message, .. } = event {
                let Message::Assistant {
                    text: Some(content),
                    ..
                } = &message
                else {
                    continue;
                };
                {
                    eprintln!("[live_test] agent replied: {content}");
                    if content.to_lowercase().contains("blue") {
                        return;
                    }
                }
            }
        }
    })
    .await;

    assert!(
        result.is_ok(),
        "timed out: agent did not reply with the PDF content within 30 s"
    );
}

/// Verify the full agent loop can reply after receiving an image input.
///
/// Run with:
///   cargo test --test live_tests -- --ignored --nocapture agent_identifies_dog_image
#[tokio::test]
#[ignore = "live: makes real NEAR AI calls, requires NEAR_API_KEY env var"]
async fn agent_identifies_dog_image() {
    let _ = dotenvy::dotenv();
    std::env::var("NEAR_API_KEY").expect("NEAR_API_KEY must be set");

    let files_dir = std::env::temp_dir().join("warden-live-test-files");
    std::fs::create_dir_all(&files_dir).unwrap();

    let bus = Bus::new(32);
    let mut events = bus.subscribe();
    let (_task, history) = start_warden(&bus, files_dir.clone());

    let image_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/live_tests/files/test.jpg"
    );
    let file_id = 123456789u32;
    let source = files_dir.join(format!("{file_id}.jpg"));
    std::fs::write(
        &source,
        std::fs::read(image_path).expect("test image should be readable"),
    )
    .unwrap();
    let message = Message::User {
        text: "Use inspect_file with query 'What animal is in this image? Answer with one word.'"
            .to_string(),
        attachments: vec![Attachment {
            file_id,
            mime_type: "image/jpeg".to_string(),
            filename: Some("test.jpg".to_string()),
            source,
        }],
    };

    publish_user_message(
        &bus,
        history.as_ref(),
        ConversationKey {
            channel: "tui".to_string(),
            chat_id: "live-test-image".to_string(),
            thread_id: None,
        },
        message,
    );

    timeout(Duration::from_secs(10), async {
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
                    if content.to_lowercase().contains("dog") {
                        return;
                    }
                }
            }
        }
    })
    .await
    .expect("timed out: agent did not reply about the test image within 30 s");
}

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

    let files_dir = std::env::temp_dir().join("warden-live-test-files");
    std::fs::create_dir_all(&files_dir).unwrap();

    let bus = Bus::new(32);
    let mut events = bus.subscribe();
    let (_task, history) = start_warden(&bus, files_dir);

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

/// Verify the agent can schedule a delayed message through an automation.
///
/// Run with:
///   cargo test --test live_tests -- --ignored --nocapture delayed_message_automation
#[tokio::test]
#[ignore = "live: makes real NEAR AI calls, requires NEAR_API_KEY env var"]
async fn delayed_message_automation() {
    let _ = dotenvy::dotenv();
    std::env::var("NEAR_API_KEY").expect("NEAR_API_KEY must be set");

    let files_dir = std::env::temp_dir().join("warden-live-test-files");
    std::fs::create_dir_all(&files_dir).unwrap();

    let bus = Bus::new(32);
    let mut events = bus.subscribe();
    let (_task, history) = start_warden(&bus, files_dir);

    let message = user_message(
        "In 3 seconds, send me a message that says exactly: BANANA. \
         The delayed message must contain only that single word.",
    );

    publish_user_message(
        &bus,
        history.as_ref(),
        ConversationKey {
            channel: "tui".to_string(),
            chat_id: "live-test-automation".to_string(),
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
                if content.trim() == "BANANA" {
                    return;
                }
            }
        }
    })
    .await;

    assert!(
        result.is_ok(),
        "timed out: the delayed BANANA message never arrived within 60 s"
    );
}

/// Verify the agent can schedule a message at an absolute wall-clock time,
/// which requires it to read the current time from its preamble.
///
/// Run with:
///   cargo test --test live_tests -- --ignored --nocapture scheduled_message_at_wall_clock_time
#[tokio::test]
#[ignore = "live: makes real NEAR AI calls, requires NEAR_API_KEY env var"]
async fn scheduled_message_at_wall_clock_time() {
    let _ = dotenvy::dotenv();
    std::env::var("NEAR_API_KEY").expect("NEAR_API_KEY must be set");

    let files_dir = std::env::temp_dir().join("warden-live-test-files");
    std::fs::create_dir_all(&files_dir).unwrap();

    let bus = Bus::new(32);
    let mut events = bus.subscribe();
    let (_task, history) = start_warden(&bus, files_dir);

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

    let files_dir = std::env::temp_dir().join("warden-live-test-files");
    std::fs::create_dir_all(&files_dir).unwrap();

    let bus = Bus::new(32);
    let mut events = bus.subscribe();
    let (_task, history) = start_warden(&bus, files_dir);

    let message = user_message(
        "Create a tool that fetches the titles of the current top 5 Hacker News \
         stories (GET https://hacker-news.firebaseio.com/v0/topstories.json for \
         the ids, then https://hacker-news.firebaseio.com/v0/item/<id>.json for \
         each title) and sends them to me in a single message that starts with \
         'HN TOP 5'. Then schedule that tool to run once, 5 seconds from now.",
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

fn user_message(text: &str) -> Message {
    Message::User {
        text: text.to_string(),
        attachments: Vec::new(),
    }
}
