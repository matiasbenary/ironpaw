use std::{io, sync::Arc};

use ironpaw::{
    agent::Agent,
    automations::{Automations, FileAutomations},
    bus::{Bus, Event},
    channels::{self, DEFAULT_ADDR},
    config::RuntimeLayout,
    history::{History, SqliteHistory},
    memory::Memory,
    message::Attachment,
    tools::{
        InternalTool, InternalTools, ToolHandler,
        browser::BrowserUpdateTool,
        dynamic::DynamicTool,
        messages::{DeleteMessageTool, SendMessageTool},
        raw_inference::ModelInference,
        setup::{
            InstallChannelTool, InstallToolTool, OnboardingTool, RemoveChannelTool, SetupServices,
        },
        state::EnabledTools,
        unlock::{AUTOMATION_NAME as UNLOCK_AUTOMATION, UnlockSecretsTool},
        wasm::WasmTool,
    },
};

#[tokio::main]
async fn main() -> io::Result<()> {
    let layout = RuntimeLayout::discover();
    let _ = dotenvy::from_path(layout.root().join(".env"));
    let token = uuid::Uuid::new_v4().to_string();
    let addr = std::env::var("IRONPAW_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());
    let bus = Bus::new(16);
    let secrets = ironpaw::secrets::Secrets::locked();
    let history: Arc<dyn History> = Arc::new(
        SqliteHistory::new(layout.data_dir().join("history.db")).map_err(std::io::Error::other)?,
    );
    let channel_runtime = Arc::new(
        channels::start_bridge(
            bus.clone(),
            Arc::clone(&history),
            &layout,
            secrets.channels.clone(),
            &addr,
            token.clone(),
        )
        .await
        .map_err(std::io::Error::other)?,
    );
    channel_runtime
        .load_only_channels(&["local-client".to_string()])
        .await
        .map_err(std::io::Error::other)?;

    let inference =
        ModelInference::new(secrets.tools.clone()).map_err(std::io::Error::other)?;
    let tools = Arc::new(InternalTools::new(
        bus.clone(),
        layout.clone(),
        inference,
        SendMessageTool::new(bus.clone(), {
            let files_dir = layout.files_dir();
            move |file_id| Attachment::from_file_id(&files_dir, file_id)
        }),
        DeleteMessageTool::new(bus.clone()),
        layout.files_dir(),
        Arc::clone(&history),
        secrets.clone(),
    ));
    let tool_handler = Arc::new(ToolHandler::new(
        bus.clone(),
        Arc::clone(&tools.registry),
        Arc::clone(&history),
    ));
    let memory = Arc::new(
        Memory::new(
            layout.data_dir().join("memory.db"),
            secrets.tools.clone(),
            tools.inference.clone(),
        )
        .map_err(io::Error::other)?,
    );
    let agent = Arc::new(Agent::new(
        bus.clone(),
        Arc::clone(&tools.registry),
        Arc::clone(&history),
        tools.inference.clone(),
        memory,
    ));
    let automation_store = Arc::new(
        FileAutomations::new(layout.data_dir().join("automations"))
            .map_err(std::io::Error::other)?,
    );
    let automations = Arc::new(Automations::new(
        bus.clone(),
        Arc::clone(&tools.registry),
        automation_store,
    ));
    let agent_handler = Arc::new(ironpaw::agent::AgentHandler::new(
        bus.clone(),
        Arc::clone(&agent),
    ));
    let boot_events = bus.subscribe();
    tools
        .registry
        .add(Arc::new(UnlockSecretsTool::new(
            bus.clone(),
            layout.clone(),
            secrets.clone(),
            Arc::clone(&tools),
        )) as Arc<dyn InternalTool>)
        .map_err(std::io::Error::other)?;
    automations
        .add_transient(UnlockSecretsTool::automation())
        .map_err(std::io::Error::other)?;
    let tool_handler_task = tool_handler.start();
    let agent_handler_task = agent_handler.start();
    let automations_task = automations.start();

    println!("unlock bridge token: {token}");
    println!("runtime root: {}", layout.root().display());
    println!("Use `ironpaw-tui {token}` to unlock Ironpaw.");

    wait_for_unlocked_store(boot_events).await?;
    automations.remove_transient(UNLOCK_AUTOMATION);
    let tool_state = EnabledTools::new(layout.tools_state_path());

    for manifest in ironpaw::tools::wasm::discover(&layout).map_err(std::io::Error::other)? {
        let name = manifest.name.clone();
        if !tool_state
            .is_enabled(&name)
            .map_err(std::io::Error::other)?
        {
            continue;
        }
        let tool = WasmTool::from_manifest(manifest, secrets.tools.clone())
            .map_err(std::io::Error::other)?;
        tools
            .registry
            .add(Arc::new(tool))
            .map_err(std::io::Error::other)?;
        println!("loaded tool: {name}");
    }
    let scripts = Arc::clone(&tools.scripts);
    for tool in DynamicTool::discover(&layout.tools_dir(), Arc::clone(&scripts))
        .map_err(std::io::Error::other)?
    {
        let name = tool.name().to_string();
        if let Err(error) = tools.registry.register_dynamic(Arc::new(tool)) {
            eprintln!("did not load dynamic tool '{name}': {error}");
        }
    }

    let setup_services = Arc::new(SetupServices::new(
        bus.clone(),
        Arc::clone(&tools),
        Arc::clone(&automations),
        layout.clone(),
        secrets.clone(),
        tool_state,
    ));
    tools
        .registry
        .add(Arc::new(OnboardingTool::new(Arc::clone(&setup_services))))
        .map_err(std::io::Error::other)?;
    let install_channel = Arc::new(InstallChannelTool::new(Arc::clone(&setup_services)));
    tools
        .registry
        .add(Arc::clone(&install_channel) as Arc<dyn InternalTool>)
        .map_err(std::io::Error::other)?;
    tools
        .registry
        .add(Arc::new(install_channel.setup_tool()))
        .map_err(std::io::Error::other)?;
    tools
        .registry
        .add(Arc::new(RemoveChannelTool::new(Arc::clone(
            &setup_services,
        ))))
        .map_err(std::io::Error::other)?;
    let install_tool = Arc::new(InstallToolTool::new(Arc::clone(&setup_services)));
    tools
        .registry
        .add(Arc::clone(&install_tool) as Arc<dyn InternalTool>)
        .map_err(std::io::Error::other)?;
    tools
        .registry
        .add(Arc::new(install_tool.setup_tool()))
        .map_err(std::io::Error::other)?;
    let stored_count = automations.load_stored().map_err(std::io::Error::other)?;
    if !automations
        .list()
        .iter()
        .any(|automation| automation.name == ironpaw::tools::browser::UPDATE_AUTOMATION_NAME)
    {
        automations
            .add_transient(BrowserUpdateTool::automation())
            .map_err(std::io::Error::other)?;
    }
    automations
        .recover_stale_setup()
        .map_err(std::io::Error::other)?;
    if stored_count == 0 {
        if secrets.exists("NEAR_API_KEY") {
            automations
                .add(Automations::handle_conversation())
                .map_err(std::io::Error::other)?;
        } else {
            automations
                .add_transient(OnboardingTool::automation())
                .map_err(std::io::Error::other)?;
        }
    }

    channel_runtime
        .load_installed_channels()
        .await
        .map_err(std::io::Error::other)?;
    setup_services.set_channel_runtime(Arc::clone(&channel_runtime));

    println!("channel bridge token: {token}");
    println!("runtime root: {}", layout.root().display());
    println!(
        "loaded secrets: {}",
        layout.root().join("secrets/secrets.json.enc").display()
    );
    let statuses = channel_runtime.statuses().await;
    if statuses.is_empty() {
        println!(
            "no channels loaded from {}; add *.wasm with matching *.config.json",
            layout.channels_dir().display()
        );
    } else {
        for status in &statuses {
            println!("loaded channel: {status}");
        }
    }
    println!(
        "ironpaw is running. Use `ironpaw-tui {token}` to send a message. Press Ctrl-C to stop."
    );
    let _tool_handler_task = tool_handler_task;
    let _agent_handler_task = agent_handler_task;
    let _automations_task = automations_task;
    tokio::signal::ctrl_c().await?;

    Ok(())
}

async fn wait_for_unlocked_store(
    mut events: tokio::sync::broadcast::Receiver<Event>,
) -> io::Result<()> {
    loop {
        match events.recv().await {
            Ok(Event::SecretsUnlocked) => return Ok(()),
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                return Err(io::Error::other("secrets unlock event bus closed"));
            }
        }
    }
}
