use std::sync::Arc;

use warden::{
    agent::Agent,
    automations::{Automations, SqliteAutomations},
    bus::Bus,
    channels::{self, DEFAULT_ADDR},
    config::RuntimeLayout,
    history::{History, SqliteHistory},
    message::Attachment,
    scripts::ScriptRuntime,
    secrets::Secrets,
    tools::{
        InternalTools, continue_conversation::ContinueConversationTool,
        delete_message::DeleteMessageTool, dynamic::DynamicTool, raw_inference::RawInferenceTool,
        send_message::SendMessageTool,
    },
};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let layout = RuntimeLayout::discover();
    let secrets = Secrets::load(&layout);

    let bus = Bus::new(16);
    let setup_complete = secrets
        .tools
        .get("NEAR_API_KEY")
        .is_some_and(|value| !value.trim().is_empty())
        && secrets
            .tools
            .get("BRAVE_SEARCH_API")
            .is_some_and(|value| !value.trim().is_empty());
    let env_path = secrets
        .loaded_env()
        .cloned()
        .unwrap_or_else(|| layout.root().join(".env"));
    let history: Arc<dyn History> = Arc::new(
        SqliteHistory::new(layout.data_dir().join("history.db")).map_err(std::io::Error::other)?,
    );
    let tools = Arc::new(InternalTools::new(
        RawInferenceTool::new().map_err(std::io::Error::other)?,
        SendMessageTool::new(bus.clone(), {
            let files_dir = layout.files_dir();
            move |file_id| Attachment::from_file_id(&files_dir, file_id)
        }),
        DeleteMessageTool::new(bus.clone()),
        layout.files_dir(),
        Arc::clone(&history),
    ));
    let agent = Arc::new(Agent::new(bus.clone(), Arc::clone(&tools)));
    tools.call_agent.bind(Arc::clone(&agent));
    let continue_conversation = Arc::new(ContinueConversationTool::new());
    continue_conversation.bind(agent);
    let scripts = Arc::new(ScriptRuntime::new(
        Arc::clone(&tools),
        continue_conversation,
    ));
    tools
        .create_tool
        .bind(Arc::clone(&tools), Arc::clone(&scripts), layout.tools_dir());
    tools
        .add(Arc::new(DynamicTool::handle_conversation(Arc::clone(
            &scripts,
        ))))
        .map_err(std::io::Error::other)?;
    for tool in DynamicTool::discover(&layout.tools_dir(), Arc::clone(&scripts))
        .map_err(std::io::Error::other)?
    {
        tools.add(Arc::new(tool)).map_err(std::io::Error::other)?;
    }
    let automation_store = Arc::new(
        SqliteAutomations::new(layout.data_dir().join("automations.db"))
            .map_err(std::io::Error::other)?,
    );
    let automations = Arc::new(Automations::new(
        bus.clone(),
        Arc::clone(&tools),
        Some(automation_store),
    ));
    tools.create_automation.bind(Arc::clone(&automations));
    tools.delete_automation.bind(Arc::clone(&automations));
    tools.inspect_automation.bind(Arc::clone(&automations));
    tools.list_automations.bind(Arc::clone(&automations));
    tools
        .onboarding
        .bind(Arc::clone(&tools), Arc::clone(&automations), env_path);
    tools.install_channel.bind(
        Arc::clone(&tools),
        Arc::clone(&automations),
        layout.clone(),
        secrets
            .loaded_env()
            .cloned()
            .unwrap_or_else(|| layout.root().join(".env")),
    );
    tools.list_channels.bind(layout.clone());
    let stored_count = automations.load_stored().map_err(std::io::Error::other)?;
    automations
        .recover_stale_channel_auth()
        .map_err(std::io::Error::other)?;
    if stored_count == 0 {
        if setup_complete {
            automations
                .add(Automations::handle_conversation())
                .map_err(std::io::Error::other)?;
        } else {
            automations
                .add(warden::tools::onboarding::OnboardingTool::automation())
                .map_err(std::io::Error::other)?;
        }
    }
    let _automations_task = automations.start();

    let token = uuid::Uuid::new_v4().to_string();
    let addr = std::env::var("WARDEN_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());
    let channel_runtime = Arc::new(
        channels::start(
            bus,
            Arc::clone(&history),
            &layout,
            secrets.channels.clone(),
            &addr,
            token.clone(),
        )
        .await
        .map_err(std::io::Error::other)?,
    );
    tools
        .install_channel
        .bind_channel_runtime(Arc::clone(&channel_runtime));

    println!("channel bridge token: {token}");
    println!("runtime root: {}", layout.root().display());
    if let Some(path) = secrets.loaded_env() {
        println!("loaded env: {}", path.display());
    } else {
        println!("loaded env: <none>");
    }
    if channel_runtime.statuses().is_empty() {
        println!(
            "no channels loaded from {}; add *.wasm with matching *.config.json",
            layout.channels_dir().display()
        );
    } else {
        for status in channel_runtime.statuses() {
            println!("loaded channel: {status}");
        }
    }
    println!(
        "warden is running. Use `warden-tui {token}` to send a message. Press Ctrl-C to stop."
    );
    tokio::signal::ctrl_c().await?;

    Ok(())
}
