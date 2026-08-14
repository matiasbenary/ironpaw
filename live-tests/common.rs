pub(crate) use std::{collections::HashSet, time::Duration};
pub(crate) use std::{path::PathBuf, sync::Arc};

pub(crate) use ironpaw::{
    agent::{Agent, AgentHandler},
    automations::{Automation, AutomationEffect, Automations, FileAutomations, Schedule},
    bus::{AutomationAction, Bus, ConversationKey, Event, MessageIn, Sender},
    channels::append_inbound_message,
    history::{History, HistoryEntry, HistoryId, MessageMetadata, SqliteHistory},
    inference::Inference,
    memory::Memory,
    message::{Attachment, Message},
    tools::{
        InternalTools, ToolHandler,
        messages::{DeleteMessageTool, SendMessageTool},
        setup::{OnboardingTool, SetupServices},
        wasm::{self, WasmTool},
    },
};
pub(crate) use tokio::time::{sleep, timeout};

pub(crate) fn start_ironpaw(
    bus: &Bus,
    files_dir: PathBuf,
) -> (tokio::task::JoinHandle<()>, Arc<dyn History>) {
    let (task, history, _) = start_ironpaw_with_automations(bus, files_dir);
    (task, history)
}

pub(crate) fn start_ironpaw_with_automations(
    bus: &Bus,
    files_dir: PathBuf,
) -> (
    tokio::task::JoinHandle<()>,
    Arc<dyn History>,
    Arc<Automations>,
) {
    let (task, history, automations, _, _) = start_ironpaw_config(bus, files_dir);
    (task, history, automations)
}

pub(crate) fn start_ironpaw_config(
    bus: &Bus,
    files_dir: PathBuf,
) -> (
    tokio::task::JoinHandle<()>,
    Arc<dyn History>,
    Arc<Automations>,
    Arc<Memory>,
    PathBuf,
) {
    reset_raw_inference_log();
    let history = temp_history();
    let layout = ironpaw::config::RuntimeLayout::from_root(std::env::temp_dir().join(format!(
        "ironpaw-live-test-runtime-{}",
        uuid::Uuid::new_v4()
    )));
    let automation_files_dir = layout.files_dir();
    install_test_telegram_channel(&layout);
    let secrets = ironpaw::secrets::Secrets::for_tests();
    if let Ok(near_api_key) = std::env::var("NEAR_API_KEY") {
        secrets.set("NEAR_API_KEY", &near_api_key).unwrap();
    }
    secrets
        .set("TELEGRAM_BOT_TOKEN", "live-test-token")
        .unwrap();
    secrets
        .set("IRONPAW_NGROK_TOKEN", "live-test-token")
        .unwrap();
    if let Ok(brave_search_api) = std::env::var("BRAVE_SEARCH_API") {
        secrets.set("BRAVE_SEARCH_API", &brave_search_api).unwrap();
    }
    let inference = Inference::new(secrets.tools.clone()).expect("create inference");
    let tools = InternalTools::new(
        bus.clone(),
        layout.clone(),
        inference,
        send_message_tool(bus, files_dir.clone()),
        DeleteMessageTool::new(bus.clone()),
        files_dir,
        Arc::clone(&history),
        secrets.clone(),
    );
    let tools = Arc::new(tools);
    let tool_handler = Arc::new(ToolHandler::new(bus.clone(), Arc::clone(&tools.registry)));
    let memory = Arc::new(
        Memory::new(
            layout.data_dir().join("memory.db"),
            secrets.tools.clone(),
            tools.inference.clone(),
        )
        .expect("create native memory"),
    );
    let agent = Agent::new(
        bus.clone(),
        Arc::clone(&tools.registry),
        Arc::clone(&history),
        tools.inference.clone(),
        Arc::clone(&memory),
    );
    let agent = Arc::new(agent);
    let automation_store = Arc::new(
        FileAutomations::new(layout.data_dir().join("automations"))
            .expect("temp automations store"),
    );
    let automations = Arc::new(Automations::new(
        bus.clone(),
        Arc::clone(&tools.registry),
        automation_store,
    ));
    let agent_handler = Arc::new(AgentHandler::new(bus.clone(), Arc::clone(&agent)));
    install_web_search_for_live_tests(&layout, &tools, &secrets);
    let tool_state = ironpaw::tools::state::EnabledTools::new(layout.tools_state_path());
    let setup_services = Arc::new(SetupServices::new(
        bus.clone(),
        Arc::clone(&tools),
        Arc::clone(&automations),
        layout,
        secrets,
        tool_state,
    ));
    tools
        .registry
        .add(
            Arc::new(OnboardingTool::new(setup_services)),
            ironpaw::tools::ToolVisibility::Internal,
        )
        .expect("onboarding registers");
    let _tool_handler_task = tool_handler.start();
    let _agent_handler_task = agent_handler.start();
    (
        automations.start(),
        history,
        automations,
        memory,
        automation_files_dir,
    )
}

fn install_test_telegram_channel(layout: &ironpaw::config::RuntimeLayout) {
    let channels_dir = layout.channels_dir();
    std::fs::create_dir_all(&channels_dir).expect("live test channels directory");
    std::fs::write(channels_dir.join("telegram.wasm"), []).expect("live test telegram wasm");
    std::fs::write(
        channels_dir.join("telegram.config.json"),
        r#"{
  "path": "/webhook/telegram",
  "default_conversation": {
    "channel": "telegram",
    "chat_id": "live-test-telegram",
    "thread_id": null
  }
}"#,
    )
    .expect("live test telegram config");
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
    let package_dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("wasm-tools/web-search");
    std::fs::copy(
        package_dir.join("web-search.wasm"),
        tools_dir.join("web_search.wasm"),
    )
    .expect("copy web_search wasm");
    std::fs::copy(
        package_dir.join("web-search.config.template.json"),
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
        .add(Arc::new(tool), ironpaw::tools::ToolVisibility::Agent)
        .expect("register web_search tool");
}

fn temp_history() -> Arc<dyn History> {
    let path =
        std::env::temp_dir().join(format!("ironpaw-test-history-{}.db", uuid::Uuid::new_v4()));
    Arc::new(SqliteHistory::new(path).expect("temp history"))
}

pub(crate) fn publish_user_message(
    bus: &Bus,
    history: &dyn History,
    conversation: ConversationKey,
    message: Message,
) {
    append_inbound_message(history, &conversation, None, &message)
        .expect("persist live test user message");
    let Message::User { text, attachments } = message else {
        panic!("inbound live test message must be a user message");
    };
    bus.publish(Event::MessageIn(MessageIn {
        sender: Sender::User("live-test".to_string()),
        conversation,
        message_id: None,
        text,
        attachments,
    }));
}

pub(crate) fn publish_guest_message(
    bus: &Bus,
    history: &dyn History,
    conversation: ConversationKey,
    text: &str,
) {
    let message = user_message(text);
    append_inbound_message(history, &conversation, None, &message)
        .expect("persist live test guest message");
    bus.publish(Event::MessageIn(MessageIn {
        sender: Sender::Guest("live-test-guest".to_string()),
        conversation,
        message_id: None,
        text: text.to_string(),
        attachments: Vec::new(),
    }));
}

fn send_message_tool(bus: &Bus, files_dir: PathBuf) -> SendMessageTool {
    SendMessageTool::new(bus.clone(), move |filename, ctx| {
        let scoped = ironpaw::message::execution_files_dir(&files_dir, ctx);
        Attachment::from_filename(&scoped, filename)
            .or_else(|_| Attachment::from_filename(&files_dir, filename))
    })
}

#[derive(serde::Deserialize, serde::Serialize)]
pub(crate) struct MemoryFixtureMessage {
    pub(crate) role: String,
    pub(crate) content: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
pub(crate) struct MemoryEvalProbe {
    pub(crate) name: String,
    pub(crate) question: String,
    pub(crate) required: Vec<String>,
    pub(crate) forbidden: Vec<String>,
}

#[derive(serde::Deserialize)]
pub(crate) struct MemoryEvalFixture {
    pub(crate) messages: Vec<MemoryFixtureMessage>,
    pub(crate) probes: Vec<MemoryEvalProbe>,
}

pub(crate) fn memory_eval_fixture() -> MemoryEvalFixture {
    serde_json::from_str(include_str!("fixtures/memory_conversation.json"))
        .expect("parse memory evaluation fixture")
}

pub(crate) fn seed_cold_history(
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
        .save_conversation_summary(
            &HistoryId::Conversation(conversation.clone()),
            "Older conversation was compacted.",
            &entries.last().expect("seeded history entry").metadata.id,
        )
        .expect("make seeded history cold");
    assert_cold_history(history, conversation);
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

pub(crate) fn assert_cold_history(history: &dyn History, conversation: &ConversationKey) {
    let active = history
        .load(conversation)
        .expect("load compacted memory evaluation history");
    assert_eq!(
        active.len(),
        1,
        "compacted history must expose only its generic summary"
    );
    assert!(
        matches!(
            &active[0].message,
            Message::System(summary) if summary == "[history summary] Older conversation was compacted."
        ),
        "compacted history must not expose the source conversation"
    );
}

pub(crate) fn missing_required_terms(memories: &[String], required: &[String]) -> Vec<String> {
    let recalled = memories.join("\n").to_lowercase();
    required
        .iter()
        .filter(|term| !recalled.contains(term.as_str()))
        .cloned()
        .collect()
}

pub(crate) fn search_history_call_count(entries: &[HistoryEntry]) -> usize {
    entries
        .iter()
        .map(|entry| match &entry.message {
            Message::Assistant { tool_calls, .. } => tool_calls
                .iter()
                .filter(|call| call.name == "search_history")
                .count(),
            _ => 0,
        })
        .sum()
}

pub(crate) async fn ask_and_wait_for_reply(
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

pub(crate) async fn ingest_fixture(memory: &Memory, messages: &[MemoryFixtureMessage]) {
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

pub(crate) async fn wait_for_memory_evidence(
    memory: &Memory,
    question: &str,
    required: &[String],
) -> Vec<String> {
    sleep(Duration::from_secs(5)).await;
    timeout(Duration::from_secs(180), async {
        loop {
            let memories = memory
                .recall(question)
                .await
                .expect("recall production-ingested memory");
            if missing_required_terms(&memories, required).is_empty() {
                return memories;
            }
            sleep(Duration::from_secs(5)).await;
        }
    })
    .await
    .expect("timed out waiting for background memory ingestion")
}

pub(crate) fn user_message(text: &str) -> Message {
    Message::User {
        text: text.to_string(),
        attachments: Vec::new(),
    }
}
