use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};

use crate::tools::{ExecutionContext, InvocationScope};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub enum Message {
    System(String),
    User {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<Attachment>,
    },
    Assistant {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<Attachment>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    ToolResult {
        tool_call_id: String,
        tool_name: String,
        result: serde_json::Value,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct Attachment {
    pub filename: String,
    pub mime_type: String,
    pub source: PathBuf,
}

impl<'de> Deserialize<'de> for Attachment {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct StoredAttachment {
            #[serde(default)]
            filename: Option<String>,
            mime_type: String,
            source: PathBuf,
        }

        let stored = StoredAttachment::deserialize(deserializer)?;
        let filename = stored.filename.unwrap_or_else(|| {
            stored
                .source
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("attachment")
                .to_string()
        });
        Ok(Self {
            filename,
            mime_type: stored.mime_type,
            source: stored.source,
        })
    }
}

impl Attachment {
    pub fn from_filename(files_dir: &Path, filename: &str) -> Result<Self, String> {
        if Path::new(filename)
            .file_name()
            .and_then(|name| name.to_str())
            != Some(filename)
        {
            return Err(format!("invalid filename: {filename}"));
        }
        let source = files_dir.join(filename);
        if !source.is_file() {
            return Err(format!("file not found: {filename}"));
        }
        let ext = source
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        Ok(Self {
            filename: filename.to_string(),
            mime_type: mime_from_ext(&ext).to_string(),
            source,
        })
    }

    pub fn read(&self) -> Result<Vec<u8>, std::io::Error> {
        std::fs::read(&self.source)
    }
}

pub fn available_filename(
    directory: &Path,
    requested: Option<&str>,
    fallback_stem: &str,
    extension: &str,
) -> String {
    let extension = extension
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .take(10)
        .collect::<String>()
        .to_ascii_lowercase();
    let extension = if extension.is_empty() {
        "bin"
    } else {
        &extension
    };
    let requested_stem = requested
        .and_then(|name| Path::new(name).file_name())
        .and_then(|name| Path::new(name).file_stem())
        .and_then(|stem| stem.to_str())
        .unwrap_or(fallback_stem);
    let mut stem = String::new();
    let mut separator = false;
    for character in requested_stem.chars() {
        if stem.len() >= 80 {
            break;
        }
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
            stem.push(character);
            separator = false;
        } else if !separator && !stem.is_empty() {
            stem.push('-');
            separator = true;
        }
    }
    let stem = stem.trim_matches('-');
    let stem = if stem.is_empty() { fallback_stem } else { stem };
    let candidate = format!("{stem}.{extension}");
    if !directory.join(&candidate).exists() {
        return candidate;
    }
    for suffix in 2.. {
        let candidate = format!("{stem}-{suffix}.{extension}");
        if !directory.join(&candidate).exists() {
            return candidate;
        }
    }
    unreachable!()
}

pub fn execution_files_dir(files_dir: &Path, ctx: &ExecutionContext) -> PathBuf {
    match &ctx.invoked_by {
        InvocationScope::Conversation(conversation) => files_dir
            .join("conversations")
            .join(scope_component(&conversation.storage_key())),
        InvocationScope::ScheduledAutomation { automation_id } => files_dir
            .join("automations")
            .join(scope_component(automation_id)),
    }
}

pub(crate) fn scope_component(name: &str) -> String {
    name.bytes().map(|byte| format!("{byte:02x}")).collect()
}

fn mime_from_ext(ext: &str) -> &'static str {
    match ext {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        _ => "application/octet-stream",
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}
