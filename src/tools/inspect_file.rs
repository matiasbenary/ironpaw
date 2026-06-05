use std::borrow::Cow;
use std::path::PathBuf;

use async_trait::async_trait;
use schemars::{JsonSchema, Schema, SchemaGenerator};

use crate::{
    message::{Attachment, Message},
    models,
    tools::{
        self, InternalTool, ToolOutput,
        ai_inference::{AiInferenceRequest, AiInferenceTool},
    },
};

pub struct InspectFileTool {
    files_dir: PathBuf,
    ai_inference: AiInferenceTool,
}

impl InspectFileTool {
    pub fn new(files_dir: PathBuf, ai_inference: AiInferenceTool) -> Self {
        Self {
            files_dir,
            ai_inference,
        }
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
struct InspectFileParams {
    file_id: u32,
    #[serde(default)]
    query: Option<String>,
}

impl JsonSchema for InspectFileParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("InspectFileParams")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        Schema::try_from(serde_json::json!({
            "type": "object",
            "properties": {
                "file_id": {
                    "type": "integer",
                    "format": "uint32",
                    "description": "The file_id of the attached file to inspect."
                },
                "query": {
                    "type": "string",
                    "description": "Optional focused question or extraction request for the file. Defaults to 'describe this file'."
                }
            },
            "required": ["file_id"]
        }))
        .expect("inspect_file schema should be valid")
    }
}

#[async_trait]
impl InternalTool for InspectFileTool {
    fn name(&self) -> &'static str {
        "inspect_file"
    }

    fn tool_definition(&self) -> crate::message::ToolDefinition {
        tools::tool_definition::<InspectFileParams>(
            self.name(),
            "Inspect an attached file by file_id. If query is omitted, it defaults to 'describe this file'. For documents and PDFs, the tool extracts readable text. For images, the tool shows the image with the query.",
        )
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput, String> {
        let params: InspectFileParams = serde_json::from_value(args).map_err(|e| e.to_string())?;

        let path = file_path(&self.files_dir, params.file_id)?;
        let data =
            std::fs::read(&path).map_err(|_| format!("file not found: {}", params.file_id))?;

        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let mime = ext_to_mime(&ext);
        let query = params.query.as_deref().unwrap_or("describe this file");

        if mime.starts_with("image/") {
            let answer = self
                .inspect_image(params.file_id, mime, path.clone(), query)
                .await?;
            return Ok(inspection_output(params.file_id, query, answer));
        }

        let text = if ext == "pdf" {
            match pdf_extract::extract_text_from_mem(&data) {
                Ok(t) => t,
                Err(_) => format!("[Could not extract text from PDF: {}]", params.file_id),
            }
        } else {
            match String::from_utf8(data) {
                Ok(t) => t,
                Err(_) => format!("[File {} is not valid UTF-8]", params.file_id),
            }
        };

        let answer = self.inspect_text(&text, query).await?;
        Ok(inspection_output(params.file_id, query, answer))
    }
}

impl InspectFileTool {
    async fn inspect_text(&self, content: &str, query: &str) -> Result<String, String> {
        let prompt = format!(
            "Query: {query}\n\nFile content:\n{content}\n\nAnswer the query using only the file content."
        );
        self.call_model(models::text_model(), vec![system_item(), text_item(prompt)])
            .await
    }

    async fn inspect_image(
        &self,
        file_id: u32,
        mime: &str,
        source: PathBuf,
        query: &str,
    ) -> Result<String, String> {
        self.call_model(
            models::vision_model(),
            vec![
                system_item(),
                Message::User {
                    text: query.to_string(),
                    attachments: vec![Attachment {
                        file_id,
                        mime_type: mime.to_string(),
                        filename: None,
                        source,
                    }],
                },
            ],
        )
            .await
    }

    async fn call_model(&self, model: String, input: Vec<Message>) -> Result<String, String> {
        let response = self
            .ai_inference
            .execute(AiInferenceRequest {
                model,
                messages: input,
                tools: Vec::new(),
                parallel_tool_calls: false,
            })
            .await?;
        let Message::Assistant { text, .. } = response else {
            return Err("file inspection model response was not an assistant message".to_string());
        };
        let answer = text.unwrap_or_default();
        if answer.trim().is_empty() {
            return Err("file inspection response was empty".to_string());
        }
        Ok(answer)
    }
}

fn system_item() -> Message {
    Message::System(
        "You inspect user-provided files and answer concisely from the file evidence.".to_string(),
    )
}

fn text_item(text: String) -> Message {
    Message::User {
        text,
        attachments: Vec::new(),
    }
}

fn file_path(files_dir: &PathBuf, file_id: u32) -> Result<PathBuf, String> {
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

fn inspection_output(file_id: u32, query: &str, answer: String) -> ToolOutput {
    ToolOutput::json(serde_json::json!({
        "file_id": file_id,
        "query": query,
        "answer": answer,
    }))
}

pub fn ext_to_mime(ext: &str) -> &str {
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
