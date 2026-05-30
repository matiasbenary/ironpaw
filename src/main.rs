use warden::{
    ai_engine::AiEngine,
    bus::Bus,
    channels::{self, DEFAULT_ADDR},
    config::RuntimeLayout,
    secrets::Secrets,
    tools::{InternalTools, ai_inference::AiInferenceTool, send_message::SendMessageTool},
};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let layout = RuntimeLayout::discover();
    let secrets = Secrets::load(&layout);

    let bus = Bus::new(16);
    let near_api_key = secrets
        .tools
        .get("NEAR_API_KEY")
        .ok_or_else(|| std::io::Error::other("missing tool secret: NEAR_API_KEY"))?;
    let tools = InternalTools::new(
        AiInferenceTool::new(near_api_key.clone()).map_err(std::io::Error::other)?,
        SendMessageTool::new(bus.clone(), layout.files_dir()),
        near_api_key.clone(),
        layout.files_dir(),
    );
    let engine = AiEngine::new(bus.clone(), tools);
    let _engine_task = engine.start();

    let token = uuid::Uuid::new_v4().to_string();
    let addr = std::env::var("WARDEN_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());
    let channel_runtime =
        channels::start(bus, &layout, secrets.channels.clone(), &addr, token.clone())
            .await
            .map_err(std::io::Error::other)?;

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
