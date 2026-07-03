use std::path::PathBuf;

use async_trait::async_trait;

use crate::{
    message::{Attachment, Message},
    models,
    tools::{
        self, InternalTool, ToolOutput,
        raw_inference::{RawInferenceRequest, RawInferenceTool},
    },
};

pub struct InspectFileTool {
    files_dir: PathBuf,
    raw_inference: RawInferenceTool,
}

impl InspectFileTool {
    pub fn new(files_dir: PathBuf, raw_inference: RawInferenceTool) -> Self {
        Self {
            files_dir,
            raw_inference,
        }
    }
}

#[derive(schemars::JsonSchema, serde::Deserialize, serde::Serialize)]
struct InspectFileParams {
    #[schemars(description = "The file_id of the attached file to inspect.")]
    file_id: u32,
    #[serde(default)]
    #[schemars(
        description = "Optional focused question or extraction request for the file. Defaults to 'describe this file'."
    )]
    query: Option<String>,
}

#[async_trait]
impl InternalTool for InspectFileTool {
    fn name(&self) -> &str {
        "inspect_file"
    }

    fn tool_definition(&self) -> crate::message::ToolDefinition {
        tools::tool_definition::<InspectFileParams>(
            self.name(),
            "Inspect an attached file by file_id. If query is omitted, it defaults to 'describe this file'. For documents and PDFs, the tool extracts readable text. For images, the tool shows the image with the query.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &crate::tools::ToolContext,
        responder: crate::tools::ToolResponder,
    ) -> Result<(), String> {
        let params: InspectFileParams = serde_json::from_value(args).map_err(|e| e.to_string())?;

        let attachment = Attachment::from_file_id(&self.files_dir, params.file_id)?;
        let path = attachment.source.clone();
        let data =
            std::fs::read(&path).map_err(|_| format!("file not found: {}", params.file_id))?;

        let ext = attachment
            .source
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let query = params.query.as_deref().unwrap_or("describe this file");

        if attachment.mime_type.starts_with("image/") {
            let answer = self.inspect_image(attachment, query).await?;
            responder.respond(inspection_output(params.file_id, query, answer))?;
            return Ok(());
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
        responder.respond(inspection_output(params.file_id, query, answer))?;
        Ok(())
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

    async fn inspect_image(&self, attachment: Attachment, query: &str) -> Result<String, String> {
        self.call_model(
            models::vision_model(),
            vec![
                system_item(),
                Message::User {
                    text: query.to_string(),
                    attachments: vec![attachment],
                },
            ],
        )
        .await
    }

    async fn call_model(&self, model: String, input: Vec<Message>) -> Result<String, String> {
        let response = self
            .raw_inference
            .execute(RawInferenceRequest {
                model,
                messages: input,
                tools: Vec::new(),
                parallel_tool_calls: false,
            })
            .await?;
        let Message::Assistant { text, .. } = response.message else {
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

fn inspection_output(_file_id: u32, _query: &str, answer: String) -> ToolOutput {
    serde_json::Value::String(answer)
}
