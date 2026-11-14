use std::{
    env,
    path::{Path, PathBuf},
    time::Duration,
};

const DEFAULT_BASE_URL: &str = "https://cloud-api.near.ai";
const DEFAULT_TEXT_MODEL: &str = "deepseek-ai/DeepSeek-V4-Flash";
const DEFAULT_VISION_MODEL: &str = "google/gemma-4-31B-it";
const DEFAULT_IMAGE_MODEL: &str = "black-forest-labs/FLUX.2-klein-4B";
const DEFAULT_EMBEDDING_MODEL: &str = "Qwen/Qwen3-Embedding-0.6B";
const DEFAULT_TRANSCRIPTION_MODEL: &str = "openai/whisper-large-v3";

#[derive(Clone, Debug)]
pub struct ProviderConfig {
    pub base_url: String,
    pub text_model: String,
    pub vision_model: String,
    pub image_model: String,
    pub embedding_model: String,
    pub transcription_model: String,
    /// A local OpenAI-compatible speech-to-text server (`whisper-server.py`) instead of NEAR AI.
    pub whisper_url: Option<String>,
    pub chat_timeout: Duration,
    pub image_timeout: Duration,
}

impl ProviderConfig {
    pub fn from_env() -> Self {
        Self {
            base_url: env::var("NEAR_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string()),
            text_model: env::var("NEAR_MODEL").unwrap_or_else(|_| DEFAULT_TEXT_MODEL.to_string()),
            vision_model: env::var("NEAR_VISION_MODEL")
                .unwrap_or_else(|_| DEFAULT_VISION_MODEL.to_string()),
            image_model: env::var("NEAR_IMAGE_MODEL")
                .unwrap_or_else(|_| DEFAULT_IMAGE_MODEL.to_string()),
            embedding_model: env::var("NEAR_EMBEDDING_MODEL")
                .unwrap_or_else(|_| DEFAULT_EMBEDDING_MODEL.to_string()),
            transcription_model: env::var("NEAR_TRANSCRIPTION_MODEL")
                .unwrap_or_else(|_| DEFAULT_TRANSCRIPTION_MODEL.to_string()),
            whisper_url: env::var("WHISPER_URL").ok().filter(|url| !url.is_empty()),
            chat_timeout: Duration::from_secs(env_u64("NEAR_CHAT_TIMEOUT_SECS", 180)),
            image_timeout: Duration::from_secs(env_u64("NEAR_IMAGE_TIMEOUT_SECS", 120)),
        }
    }
}

#[derive(Clone, Debug)]
pub struct RuntimeLayout {
    root: PathBuf,
}

impl RuntimeLayout {
    pub fn discover() -> Self {
        let root = env::var_os("IRONPAW_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(default_runtime_root);
        Self { root }
    }

    pub fn from_root(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn channels_dir(&self) -> PathBuf {
        self.root.join("channels")
    }

    pub fn tools_dir(&self) -> PathBuf {
        self.root.join("tools")
    }

    pub fn files_dir(&self) -> PathBuf {
        self.root.join("files")
    }

    pub fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }

    pub fn tools_state_path(&self) -> PathBuf {
        self.data_dir().join("tools.json")
    }

    pub fn bin_dir(&self) -> PathBuf {
        self.root.join("bin")
    }
}

fn default_runtime_root() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn env_u64(name: &str, default: u64) -> u64 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}
