use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Attachment {
    pub file_id: u32,
    pub mime_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    pub source: PathBuf,
}

impl Attachment {
    pub fn from_file_id(files_dir: &Path, file_id: u32) -> Result<Self, String> {
        let source = attachment_path(files_dir, file_id)?;
        let ext = source
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let filename = source
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string);
        Ok(Self {
            file_id,
            mime_type: mime_from_ext(&ext).to_string(),
            filename,
            source,
        })
    }

    pub fn read(&self) -> Result<Vec<u8>, std::io::Error> {
        std::fs::read(&self.source)
    }
}

fn attachment_path(files_dir: &Path, file_id: u32) -> Result<PathBuf, String> {
    let prefix = format!("{file_id}.");
    let entries = std::fs::read_dir(files_dir).map_err(|_| format!("file not found: {file_id}"))?;
    for entry in entries {
        let path = entry.map_err(|err| err.to_string())?.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(&prefix))
        {
            return Ok(path);
        }
    }
    Err(format!("file not found: {file_id}"))
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
