use std::sync::{Arc, OnceLock};

use crate::{
    automations::Automations,
    bus::Bus,
    channels,
    config::RuntimeLayout,
    secrets::Secrets,
    tools::{InternalTools, state::EnabledTools},
};

pub struct SetupServices {
    pub bus: Bus,
    pub tools: Arc<InternalTools>,
    pub automations: Arc<Automations>,
    pub layout: RuntimeLayout,
    pub secrets: Secrets,
    pub tool_state: EnabledTools,
    channel_runtime: OnceLock<Arc<channels::ChannelRuntime>>,
}

impl SetupServices {
    pub fn new(
        bus: Bus,
        tools: Arc<InternalTools>,
        automations: Arc<Automations>,
        layout: RuntimeLayout,
        secrets: Secrets,
        tool_state: EnabledTools,
    ) -> Self {
        Self {
            bus,
            tools,
            automations,
            layout,
            secrets,
            tool_state,
            channel_runtime: OnceLock::new(),
        }
    }

    pub fn set_channel_runtime(&self, runtime: Arc<channels::ChannelRuntime>) {
        let _ = self.channel_runtime.set(runtime);
    }

    pub fn channel_runtime(&self) -> Result<Arc<channels::ChannelRuntime>, String> {
        self.channel_runtime
            .get()
            .cloned()
            .ok_or_else(|| "channel runtime is not available".to_string())
    }
}
