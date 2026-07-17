use std::{collections::HashSet, time::Duration};
use std::{path::PathBuf, sync::Arc};

use tokio::time::timeout;
use ironpaw::{
    agent::{Agent, AgentHandler},
    automations::{Automations, SqliteAutomations},
    bus::{AutomationAction, Bus, ConversationKey, Event, Sender},
    history::{History, HistoryEntry, HistoryScope, MessageMetadata, SqliteHistory},
    memory::Memory,
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
    let (task, history, _) = start_warden_with_automations(bus, files_dir);
    (task, history)
}

fn start_warden_with_automations(
    bus: &Bus,
    files_dir: PathBuf,
) -> (
    tokio::task::JoinHandle<()>,
    Arc<dyn History>,
    Arc<Automations>,
) {
    let (task, history, automations, _) = start_warden_config(bus, files_dir);
    (task, history, automations)
}

fn start_warden_config(
    bus: &Bus,
    files_dir: PathBuf,
) -> (
    tokio::task::JoinHandle<()>,
    Arc<dyn History>,
    Arc<Automations>,
    Arc<Memory>,
) {
    reset_raw_inference_log();
    let history = temp_history();
    let layout = ironpaw::config::RuntimeLayout::from_root(
        std::env::temp_dir().join(format!("ironpaw-live-test-runtime-{}", uuid::Uuid::new_v4())),
    );
    let secrets = ironpaw::secrets::Secrets::for_tests();
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
    let memory = Arc::new(
        Memory::new(
            layout.data_dir().join("memory.db"),
            secrets.tools.clone(),
            tools.raw_inference.clone(),
        )
        .expect("create native memory"),
    );
    let agent = Agent::new(
        bus.clone(),
        Arc::clone(&tools.registry),
        Arc::clone(&history),
        tools.raw_inference.clone(),
        Arc::clone(&memory),
    );
    let agent = Arc::new(agent);
    let automations_db = std::env::temp_dir().join(format!(
        "ironpaw-live-test-automations-{}.db",
        uuid::Uuid::new_v4()
    ));
    let automation_store =
        Arc::new(SqliteAutomations::new(automations_db).expect("temp automations db"));
    let automations = Arc::new(Automations::new(
        bus.clone(),
        Arc::clone(&tools.registry),
        automation_store,
    ));
    let agent_handler = Arc::new(AgentHandler::new(bus.clone(), Arc::clone(&agent)));
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
    let _tool_handler_task = tool_handler.start();
    let _agent_handler_task = agent_handler.start();
    (automations.start(), history, automations, memory)
}

fn reset_raw_inference_log() {
    let path = "/Users/gagdiez/tmp/miniclaw/release/llm_requests.log";
    let _ = std::fs::write(path, "");
}

fn install_web_search_for_live_tests(
    layout: &ironpaw::config::RuntimeLayout,
    tools: &Arc<InternalTools>,
    secrets: &ironpaw::secrets::Secrets,
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
        std::env::temp_dir().join(format!("ironpaw-test-history-{}.db", uuid::Uuid::new_v4()));
    Arc::new(SqliteHistory::new(path).expect("temp history"))
}

fn publish_user_message(
    bus: &Bus,
    history: &dyn History,
    conversation: ConversationKey,
    message: Message,
) {
    history
        .append(
            &conversation,
            &[HistoryEntry {
                message: message.clone(),
                metadata: MessageMetadata::new(conversation.clone(), None),
            }],
        )
        .expect("persist live test user message");
    bus.publish(Event::MessageIn {
        sender: Sender::User("live-test".to_string()),
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

#[derive(serde::Deserialize, serde::Serialize)]
struct MemoryFixtureMessage {
    role: String,
    content: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct MemoryEvalProbe {
    name: String,
    question: String,
    required: Vec<String>,
    forbidden: Vec<String>,
}

#[derive(serde::Deserialize)]
struct MemoryEvalFixture {
    messages: Vec<MemoryFixtureMessage>,
    probes: Vec<MemoryEvalProbe>,
}

fn memory_eval_fixture() -> MemoryEvalFixture {
    serde_json::from_str(include_str!("fixtures/memory_conversation.json"))
        .expect("parse memory evaluation fixture")
}

fn seed_cold_history(
    history: &dyn History,
    conversation: &ConversationKey,
    messages: &[MemoryFixtureMessage],
) {
    let entries = messages
        .iter()
        .map(|fixture| HistoryEntry {
            message: fixture_message(fixture),
            metadata: MessageMetadata::new(conversation.clone(), None),
        })
        .collect::<Vec<_>>();
    history
        .append(conversation, &entries)
        .expect("seed memory evaluation history");
    history
        .compact_scope(
            &HistoryScope::Conversation(conversation.clone()),
            "Older conversation was compacted.",
            &[],
        )
        .expect("make seeded history cold");
}

fn fixture_message(fixture: &MemoryFixtureMessage) -> Message {
    match fixture.role.as_str() {
        "user" => user_message(&fixture.content),
        "assistant" => Message::Assistant {
            text: Some(fixture.content.clone()),
            attachments: Vec::new(),
            tool_calls: Vec::new(),
        },
        role => panic!("unsupported memory fixture role: {role}"),
    }
}

async fn ask_and_wait_for_reply(
    bus: &Bus,
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    history: &dyn History,
    conversation: ConversationKey,
    question: &str,
) -> String {
    publish_user_message(bus, history, conversation.clone(), user_message(question));
    timeout(Duration::from_secs(180), async {
        loop {
            let event = events.recv().await.expect("bus closed");
            let Event::MessageOut {
                chat_id,
                message:
                    Message::Assistant {
                        text: Some(text), ..
                    },
                ..
            } = event
            else {
                continue;
            };
            if chat_id.as_deref() == Some(conversation.chat_id.as_str())
                && !text.starts_with("calling ")
            {
                return text;
            }
        }
    })
    .await
    .expect("timed out waiting for memory evaluation reply")
}

async fn ingest_fixture(memory: &Memory, messages: &[MemoryFixtureMessage]) {
    let mut previous = Vec::new();
    for chunk in messages.chunks(20) {
        let current = chunk.iter().map(fixture_message).collect::<Vec<_>>();
        memory
            .ingest(&previous, &current)
            .await
            .expect("ingest native memory fixture");
        previous.extend(current);
        if previous.len() > 10 {
            previous.drain(..previous.len() - 10);
        }
    }
}

/// Run the cold-history scenarios with Warden's native automatic memory recall.
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
    let (_task, history, _, memory) = start_warden_config(&bus, files_dir);
    let mut passed = 0;
    let mut evaluated = 0;
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
        let searched = history
            .load(&conversation)
            .expect("load memory evaluation transcript")
            .iter()
            .any(|entry| match &entry.message {
                Message::Assistant { tool_calls, .. } => {
                    tool_calls.iter().any(|call| call.name == "search_history")
                }
                _ => false,
            });
        passed += usize::from(correct);
        eprintln!(
            "[memory_eval] case={} correct={} searched={} memories={:?} latency_ms={} reply={:?}",
            case.name,
            correct,
            searched,
            memories,
            started.elapsed().as_millis(),
            reply,
        );
    }

    eprintln!("[memory_eval] score={passed}/{}", evaluated);
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

    let files_dir = std::env::temp_dir().join("ironpaw-live-test-files");
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

    let files_dir = std::env::temp_dir().join("ironpaw-live-test-files");
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
    let files_dir = std::env::temp_dir().join("ironpaw-live-test-files");
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

    let files_dir = std::env::temp_dir().join("ironpaw-live-test-files");
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
    let pdf_path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests_live/files/test.pdf");
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

    let files_dir = std::env::temp_dir().join("ironpaw-live-test-files");
    std::fs::create_dir_all(&files_dir).unwrap();

    let bus = Bus::new(32);
    let mut events = bus.subscribe();
    let (_task, history) = start_warden(&bus, files_dir.clone());

    let image_path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests_live/files/test.jpg");
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

    let files_dir = std::env::temp_dir().join("ironpaw-live-test-files");
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
    let (_task, history) = start_warden(&bus, files_dir);
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

/// Verify the agent can schedule a delayed message through an automation.
///
/// Run with:
///   cargo test --test live_tests -- --ignored --nocapture delayed_message_automation
#[tokio::test]
#[ignore = "live: makes real NEAR AI calls, requires NEAR_API_KEY env var"]
async fn delayed_message_automation() {
    let _ = dotenvy::dotenv();
    std::env::var("NEAR_API_KEY").expect("NEAR_API_KEY must be set");

    let files_dir = std::env::temp_dir().join("ironpaw-live-test-files");
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

    let files_dir = std::env::temp_dir().join("ironpaw-live-test-files");
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
    let (_task, history, automations) = start_warden_with_automations(&bus, files_dir);
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
            if let Some(automation) = automations
                .list()
                .into_iter()
                .find(|automation| !existing.contains(&automation.name))
            {
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
                    .and_then(|args| args["content"].as_str().map(str::to_string))
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

/// Verify the generated daily album automation returns different recommendations across runs.
///
/// Run with:
///   cargo test --test live_tests -- --ignored --nocapture --test-threads=1 album_recommendation_automation
#[tokio::test]
#[ignore = "live: observational test that makes real NEAR AI calls"]
async fn album_recommendation_automation() {
    let _ = dotenvy::dotenv();
    std::env::var("NEAR_API_KEY").expect("NEAR_API_KEY must be set");

    let files_dir = std::env::temp_dir().join("ironpaw-live-test-files");
    std::fs::create_dir_all(&files_dir).unwrap();

    let bus = Bus::new(32);
    let mut events = bus.subscribe();
    let (_task, history, automations) = start_warden_with_automations(&bus, files_dir);
    let existing = automations
        .list()
        .into_iter()
        .map(|automation| automation.name)
        .collect::<HashSet<_>>();

    let prompt = r#"I am trying to listing to new bands, the more distant from my usual experience the better. The idea is not to listen to crap, but to move away from the mainstream influx of music we get bombarded with, which sounds all the same. Here are the albums I already listened to:

https://docs.google.com/spreadsheets/d/1bgDum7fxXzIo5bRDJzn7dy33s8bcPZxrf1YF1-jEy1c/edit?usp=sharing

rules:
- prefer non-english speaking bands
- bad candidates are obvious bands (e.g. daft punk, the cure, pink floyd)

suggest me one new album to listen every day at 10am, including the band name and album name
"#;

    publish_user_message(
        &bus,
        history.as_ref(),
        ConversationKey {
            channel: "tui".to_string(),
            chat_id: "live-album-recommendation".to_string(),
            thread_id: None,
        },
        user_message(prompt),
    );

    let generated = timeout(Duration::from_secs(300), async {
        loop {
            let generated = automations
                .list()
                .into_iter()
                .filter(|automation| !existing.contains(&automation.name))
                .collect::<Vec<_>>();
            if !generated.is_empty() {
                break generated;
            }
            if let Ok(Ok(Event::MessageOut {
                chat_id,
                message:
                    Message::Assistant {
                        text: Some(text), ..
                    },
                ..
            })) = timeout(Duration::from_millis(100), events.recv()).await
                && chat_id.as_deref() == Some("live-album-recommendation")
                && !text.starts_with("calling ")
            {
                eprintln!("[live_test] agent response: {text}");
                break automations
                    .list()
                    .into_iter()
                    .filter(|automation| !existing.contains(&automation.name))
                    .collect();
            }
        }
    })
    .await
    .unwrap_or_default();

    if generated.is_empty() {
        panic!("agent did not create an album automation");
    } else {
        eprintln!(
            "[live_test] generated album automation:\n{}",
            serde_json::to_string_pretty(&generated).expect("serialize generated automation")
        );
    }

    let automation_name = &generated[0].name;
    let first = trigger_automation_and_wait(&bus, &mut events, automation_name).await;
    let second = trigger_automation_and_wait(&bus, &mut events, automation_name).await;
    eprintln!("[live_test] first recommendation: {first:?}");
    eprintln!("[live_test] second recommendation: {second:?}");
    assert_ne!(
        first, second,
        "album automation repeated its recommendation"
    );
}

async fn trigger_automation_and_wait(
    bus: &Bus,
    events: &mut tokio::sync::broadcast::Receiver<Event>,
    automation_name: &str,
) -> Option<String> {
    let request_id = uuid::Uuid::new_v4().to_string();
    bus.publish(Event::Automation {
        id: request_id.clone(),
        action: AutomationAction::Trigger {
            name: automation_name.to_string(),
        },
    });
    let execution_id = loop {
        if let Event::AutomationResult { id, result } = events.recv().await.expect("event")
            && id == request_id
        {
            let result = result.expect("trigger automation");
            break result["execution_id"]
                .as_str()
                .expect("execution id")
                .to_string();
        }
    };
    loop {
        if let Event::AgentResult {
            execution_id: result_id,
            result,
        } = events.recv().await.expect("event")
            && result_id == execution_id
        {
            return result.expect("automation agent result");
        }
    }
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

    let files_dir = std::env::temp_dir().join("ironpaw-live-test-files");
    std::fs::create_dir_all(&files_dir).unwrap();

    let bus = Bus::new(32);
    let mut events = bus.subscribe();
    let (_task, history) = start_warden(&bus, files_dir);

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

fn user_message(text: &str) -> Message {
    Message::User {
        text: text.to_string(),
        attachments: Vec::new(),
    }
}
