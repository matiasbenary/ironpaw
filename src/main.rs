use std::{io, sync::Arc};

use ironpaw::{
    agent::Agent,
    automations::{Automations, FileAutomations},
    bus::{Bus, Event},
    channels::{self, DEFAULT_ADDR},
    config::RuntimeLayout,
    history::{History, SqliteHistory},
    inference::Inference,
    memory::Memory,
    message::Attachment,
    tools::{
        InternalTool, InternalTools, ToolHandler, ToolVisibility,
        browser::BrowserUpdateTool,
        dynamic::DynamicTool,
        messages::{DeleteMessageTool, SendMessageTool},
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
    let mut arguments = std::env::args_os();
    let _program = arguments.next();
    if arguments.next().as_deref()
        == Some(std::ffi::OsStr::new(
            ironpaw::shell_tools::proxy_bridge::ARGUMENT,
        ))
    {
        let status = ironpaw::shell_tools::proxy_bridge::run(arguments.collect())
            .await
            .map_err(io::Error::other)?;
        std::process::exit(status);
    }

    let layout = RuntimeLayout::discover();
    let _ = dotenvy::from_path(layout.root().join(".env"));
    let addr = std::env::var("IRONPAW_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());
    let bus = Bus::new(512);
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
        )
        .await
        .map_err(std::io::Error::other)?,
    );
    let token = channel_runtime.connection_token().to_string();
    let provider = Arc::new(ironpaw::config::ProviderConfig::from_env());
    let inference = Inference::with_config(secrets.tools.clone(), Arc::clone(&provider))
        .map_err(std::io::Error::other)?;
    let tools = Arc::new(InternalTools::new(
        bus.clone(),
        layout.clone(),
        inference,
        SendMessageTool::new(bus.clone(), {
            let files_dir = layout.files_dir();
            move |filename, ctx| {
                let scoped = ironpaw::message::execution_files_dir(&files_dir, ctx);
                Attachment::from_filename(&scoped, filename)
                    .or_else(|_| Attachment::from_filename(&files_dir, filename))
            }
        }),
        DeleteMessageTool::new(bus.clone()),
        layout.files_dir(),
        Arc::clone(&history),
        secrets.clone(),
    ));
    let tool_handler = Arc::new(ToolHandler::new(bus.clone(), Arc::clone(&tools.registry)));
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
        .add(
            Arc::new(UnlockSecretsTool::new(
                bus.clone(),
                layout.clone(),
                secrets.clone(),
                Arc::clone(&tools),
            )) as Arc<dyn InternalTool>,
            ToolVisibility::Internal,
        )
        .map_err(std::io::Error::other)?;
    automations
        .add_runtime(UnlockSecretsTool::automation())
        .map_err(std::io::Error::other)?;
    let tool_handler_task = tool_handler.start();
    let agent_handler_task = agent_handler.start();
    let automations_task = automations.start();

    println!("local client connection token: {token}");
    println!("runtime root: {}", layout.root().display());
    println!("Use `ironpaw-tui {token}` to unlock Ironpaw.");

    wait_for_unlocked_store(boot_events).await?;
    automations.remove_runtime(UNLOCK_AUTOMATION);
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
            .add(Arc::new(tool), ToolVisibility::Agent)
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
    let onboarding = Arc::new(OnboardingTool::new(Arc::clone(&setup_services)));
    tools
        .registry
        .add(
            Arc::clone(&onboarding) as Arc<dyn InternalTool>,
            ToolVisibility::Internal,
        )
        .map_err(std::io::Error::other)?;
    let onboarding_task = onboarding.start();
    let install_channel = Arc::new(InstallChannelTool::new(Arc::clone(&setup_services)));
    tools
        .registry
        .add(
            Arc::clone(&install_channel) as Arc<dyn InternalTool>,
            ToolVisibility::Agent,
        )
        .map_err(std::io::Error::other)?;
    tools
        .registry
        .add(
            Arc::new(install_channel.setup_tool()),
            ToolVisibility::Internal,
        )
        .map_err(std::io::Error::other)?;
    tools
        .registry
        .add(
            Arc::new(RemoveChannelTool::new(Arc::clone(&setup_services))),
            ToolVisibility::Agent,
        )
        .map_err(std::io::Error::other)?;
    let install_tool = Arc::new(InstallToolTool::new(Arc::clone(&setup_services)));
    tools
        .registry
        .add(
            Arc::clone(&install_tool) as Arc<dyn InternalTool>,
            ToolVisibility::Agent,
        )
        .map_err(std::io::Error::other)?;
    tools
        .registry
        .add(
            Arc::new(install_tool.setup_tool()),
            ToolVisibility::Internal,
        )
        .map_err(std::io::Error::other)?;
    tools
        .registry
        .add(
            Arc::new(install_tool.configure_tool()),
            ToolVisibility::Agent,
        )
        .map_err(std::io::Error::other)?;
    automations.load_stored().map_err(std::io::Error::other)?;
    if !automations
        .list()
        .iter()
        .any(|automation| automation.name() == ironpaw::tools::browser::UPDATE_AUTOMATION_NAME)
    {
        automations
            .add_runtime(BrowserUpdateTool::automation())
            .map_err(std::io::Error::other)?;
    }
    if !secrets.exists("NEAR_API_KEY") {
        automations
            .add_runtime(OnboardingTool::automation())
            .map_err(std::io::Error::other)?;
    }

    channel_runtime
        .load_installed_channels()
        .await
        .map_err(std::io::Error::other)?;
    setup_services.set_channel_runtime(Arc::clone(&channel_runtime));

    println!("local client connection token: {token}");
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
    let _onboarding_task = onboarding_task;
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
