use std::{
    path::PathBuf,
    sync::{Arc, OnceLock},
};

use crate::{automations::Automations, channels, config::RuntimeLayout, tools::InternalTools};

pub struct SetupServices {
    pub tools: Arc<InternalTools>,
    pub automations: Arc<Automations>,
    pub layout: RuntimeLayout,
    pub env_path: PathBuf,
    channel_runtime: OnceLock<Arc<channels::ChannelRuntime>>,
}

impl SetupServices {
    pub fn new(
        tools: Arc<InternalTools>,
        automations: Arc<Automations>,
        layout: RuntimeLayout,
        env_path: PathBuf,
    ) -> Self {
        Self {
            tools,
            automations,
            layout,
            env_path,
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
