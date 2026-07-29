use std::path::PathBuf;

use async_trait::async_trait;

use crate::{
    inference::{Inference, InferenceRequest},
    message::{Attachment, Message},
    tools::{self, InternalTool},
};

pub struct InspectFileTool {
    file_dirs: Vec<PathBuf>,
    inference: Inference,
}

impl InspectFileTool {
    pub fn new(file_dirs: Vec<PathBuf>, inference: Inference) -> Self {
        Self {
            file_dirs,
            inference,
        }
    }
}

#[derive(schemars::JsonSchema, serde::Deserialize, serde::Serialize)]
struct InspectFileParams {
    #[schemars(description = "The filename of the attached or downloaded file to inspect.")]
    filename: String,
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
            "Inspect a stored file by filename. If query is omitted, it defaults to 'describe this file'. PDFs use text extraction, text files are passed through as bounded raw content, and images use vision.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &crate::tools::ExecutionContext,
        responder: crate::tools::ToolResponder,
    ) {
        let result: Result<serde_json::Value, String> = async {
            let params: InspectFileParams =
                serde_json::from_value(args).map_err(|e| e.to_string())?;

            let attachment = self
                .file_dirs
                .iter()
                .find_map(|dir| Attachment::from_filename(dir, &params.filename).ok())
                .ok_or_else(|| format!("file not found: {}", params.filename))?;
            let path = attachment.source.clone();
            let data =
                std::fs::read(&path).map_err(|_| format!("file not found: {}", params.filename))?;

            let ext = attachment
                .source
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let query = params.query.as_deref().unwrap_or("describe this file");

            if attachment.mime_type.starts_with("image/") {
                let answer = self
                    .inspect_image(attachment, query, ctx.conversation())
                    .await?;
                return Ok(serde_json::Value::String(answer));
            }

            let text = if ext == "pdf" {
                match pdf_extract::extract_text_from_mem(&data) {
                    Ok(t) => t,
                    Err(_) => format!("[Could not extract text from PDF: {}]", params.filename),
                }
            } else {
                bounded_raw_text(&data)
            };

            let answer = self.inspect_text(&text, query, ctx.conversation()).await?;
            Ok(serde_json::Value::String(answer))
        }
        .await;
        responder.respond(result.map_err(crate::tools::ToolError::new));
    }
}

impl InspectFileTool {
    async fn inspect_text(
        &self,
        content: &str,
        query: &str,
        conversation: Option<&crate::bus::ConversationKey>,
    ) -> Result<String, String> {
        let prompt = format!(
            "Query: {query}\n\nFile content:\n{content}\n\nAnswer the query using only the file content."
        );
        self.call_model(
            self.inference.provider().text_model.clone(),
            vec![system_item(), text_item(prompt)],
            conversation,
        )
        .await
    }

    async fn inspect_image(
        &self,
        attachment: Attachment,
        query: &str,
        conversation: Option<&crate::bus::ConversationKey>,
    ) -> Result<String, String> {
        self.call_model(
            self.inference.provider().vision_model.clone(),
            vec![
                system_item(),
                Message::User {
                    text: query.to_string(),
                    attachments: vec![attachment],
                },
            ],
            conversation,
        )
        .await
    }

    async fn call_model(
        &self,
        model: String,
        input: Vec<Message>,
        conversation: Option<&crate::bus::ConversationKey>,
    ) -> Result<String, String> {
        let response = self
            .inference
            .execute(
                InferenceRequest {
                    model,
                    messages: input,
                    tools: Vec::new(),
                },
                conversation,
            )
            .await
            .map_err(|error| error.to_string())?;
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

fn bounded_raw_text(data: &[u8]) -> String {
    const MAX_TEXT_BYTES: usize = 100_000;
    let text = String::from_utf8_lossy(data);
    let mut end = text.len().min(MAX_TEXT_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut output = text[..end].to_string();
    if end < text.len() {
        output.push_str("\n[File content truncated at 100000 bytes]");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::bounded_raw_text;

    #[test]
    fn preserves_raw_csv_content() {
        let content = bounded_raw_text(b"artist,album\n\"A, B\",Record\n");
        assert_eq!(content, "artist,album\n\"A, B\",Record\n");
    }

    #[test]
    fn preserves_malformed_csv_as_lossy_text() {
        let content = bounded_raw_text(b"artist,album\nunfinished,\xff\n");
        assert!(content.contains("artist,album\nunfinished,"));
    }
}
