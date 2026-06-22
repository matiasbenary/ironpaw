use std::sync::Arc;

use warden::{
    agent::Agent,
    automations::{Automations, SqliteAutomations},
    bus::Bus,
    channels::{self, DEFAULT_ADDR},
    config::RuntimeLayout,
    history::{History, SqliteHistory},
    message::Attachment,
    secrets::Secrets,
    tools::{
        InternalTools,
        dynamic::DynamicTool,
        messages::{DeleteMessageTool, SendMessageTool},
        raw_inference::RawInferenceTool,
        setup::{InstallChannelTool, OnboardingTool, SetupServices},
    },
};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let layout = RuntimeLayout::discover();
    let secrets = Secrets::load(&layout);

    let bus = Bus::new(16);
    let history: Arc<dyn History> = Arc::new(
        SqliteHistory::new(layout.data_dir().join("history.db")).map_err(std::io::Error::other)?,
    );
    let tools = Arc::new(InternalTools::new(
        bus.clone(),
        layout.clone(),
        RawInferenceTool::new().map_err(std::io::Error::other)?,
        SendMessageTool::new(bus.clone(), {
            let files_dir = layout.files_dir();
            move |file_id| Attachment::from_file_id(&files_dir, file_id)
        }),
        DeleteMessageTool::new(bus.clone()),
        layout.files_dir(),
        Arc::clone(&history),
    ));
    let agent = Arc::new(Agent::new(
        bus.clone(),
        Arc::clone(&tools.registry),
        Arc::clone(&history),
        tools.raw_inference.clone(),
    ));
    let scripts = Arc::clone(&tools.scripts);
    for tool in DynamicTool::discover(&layout.tools_dir(), Arc::clone(&scripts))
        .map_err(std::io::Error::other)?
    {
        tools
            .registry
            .add(Arc::new(tool))
            .map_err(std::io::Error::other)?;
    }
    let automation_store = Arc::new(
        SqliteAutomations::new(layout.data_dir().join("automations.db"))
            .map_err(std::io::Error::other)?,
    );
    let automations = Arc::new(Automations::new(
        bus.clone(),
        Arc::clone(&tools),
        Arc::clone(&agent),
        automation_store,
    ));
    let env_path = secrets
        .loaded_env()
        .cloned()
        .unwrap_or_else(|| layout.root().join(".env"));
    let setup_services = Arc::new(SetupServices::new(
        Arc::clone(&tools),
        Arc::clone(&automations),
        layout.clone(),
        env_path,
    ));
    tools
        .registry
        .add(Arc::new(OnboardingTool::new(Arc::clone(&setup_services))))
        .map_err(std::io::Error::other)?;
    tools
        .registry
        .add(Arc::new(InstallChannelTool::new(Arc::clone(
            &setup_services,
        ))))
        .map_err(std::io::Error::other)?;
    let stored_count = automations.load_stored().map_err(std::io::Error::other)?;
    automations
        .recover_stale_channel_auth()
        .map_err(std::io::Error::other)?;
    if stored_count == 0 {
        let setup_complete = secrets
            .tools
            .get("NEAR_API_KEY")
            .is_some_and(|value| !value.trim().is_empty())
            && secrets
                .tools
                .get("BRAVE_SEARCH_API")
                .is_some_and(|value| !value.trim().is_empty());
        if setup_complete {
            automations
                .add(Automations::handle_conversation())
                .map_err(std::io::Error::other)?;
        } else {
            automations
                .add(OnboardingTool::automation())
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
    setup_services.set_channel_runtime(Arc::clone(&channel_runtime));

    println!("channel bridge token: {token}");
    println!("runtime root: {}", layout.root().display());
    if let Some(path) = secrets.loaded_env() {
        println!("loaded env: {}", path.display());
    } else {
        println!("loaded env: <none>");
    }
    if channel_runtime.statuses.is_empty() {
        println!(
            "no channels loaded from {}; add *.wasm with matching *.config.json",
            layout.channels_dir().display()
        );
    } else {
        for status in &channel_runtime.statuses {
            println!("loaded channel: {status}");
        }
    }
    println!(
        "warden is running. Use `warden-tui {token}` to send a message. Press Ctrl-C to stop."
    );
    tokio::signal::ctrl_c().await?;

    Ok(())
}
