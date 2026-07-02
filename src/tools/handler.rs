use std::sync::Arc;

use crate::{
    bus::{Bus, Event, ToolOrigin},
    tools::{ToolContext, ToolRegistry, run_registered_tool_for_context},
};

pub struct ToolHandler {
    bus: Bus,
    registry: Arc<ToolRegistry>,
}

impl ToolHandler {
    pub fn new(bus: Bus, registry: Arc<ToolRegistry>) -> Self {
        Self { bus, registry }
    }

    pub fn start(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = Arc::clone(self);
        let mut events = self.bus.subscribe();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(Event::ToolRequest {
                        origin,
                        tool_call_id,
                        tool_name,
                        args,
                    }) => this.spawn_tool_request(origin, tool_call_id, tool_name, args),
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }

    fn spawn_tool_request(
        &self,
        origin: ToolOrigin,
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
    ) {
        let bus = self.bus.clone();
        let registry = Arc::clone(&self.registry);
        tokio::spawn(async move {
            let ctx = ToolContext::for_request(bus, origin, tool_call_id, tool_name);
            run_registered_tool_for_context(&registry, &ctx, args).await;
        });
    }
}
