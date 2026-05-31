use std::borrow::Cow;
use std::path::PathBuf;

use async_trait::async_trait;
use base64::Engine as _;
use schemars::{JsonSchema, Schema, SchemaGenerator};

use crate::openai::{FunctionCallOutput, ImageDetail, InputContent, InputImageContent};
use crate::tools::{self, InternalTool, ToolOutput};

pub struct ReadFileTool {
    files_dir: PathBuf,
}

impl ReadFileTool {
    pub fn new(files_dir: PathBuf) -> Self {
        Self { files_dir }
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
struct ReadFileParams {
    file_id: String,
}

impl JsonSchema for ReadFileParams {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("ReadFileParams")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        Schema::try_from(serde_json::json!({
            "type": "object",
            "properties": {
                "file_id": {
                    "type": "string",
                    "description": "The file_id of the attached file to read."
                }
            },
            "required": ["file_id"]
        }))
        .expect("read_file schema should be valid")
    }
}

#[async_trait]
impl InternalTool for ReadFileTool {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn responses_schema(&self) -> crate::openai::Tool {
        tools::responses_tool_schema::<ReadFileParams>(
            self.name(),
            "Read the content of an attached file by its file_id. \
            Returns text for documents and PDFs, or displays images inline.",
        )
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolOutput, String> {
        let params: ReadFileParams =
            serde_json::from_value(args).map_err(|e| e.to_string())?;

        // file_id is stored as "<uuid>.<ext>" so the path is direct.
        let path = self.files_dir.join(&params.file_id);
        let data = std::fs::read(&path)
            .map_err(|_| format!("file not found: {}", params.file_id))?;

        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let mime = ext_to_mime(&ext);

        if mime.starts_with("image/") {
            let data_url = format!(
                "data:{};base64,{}",
                mime,
                base64::engine::general_purpose::STANDARD.encode(&data)
            );
            return Ok(ToolOutput {
                model_output: FunctionCallOutput::Content(vec![
                    InputContent::InputImage(InputImageContent {
                        detail: ImageDetail::Auto,
                        file_id: None,
                        image_url: Some(data_url),
                    }),
                ]),
                files: Vec::new(),
            });
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

        Ok(ToolOutput::json(serde_json::json!({
            "file_id": params.file_id,
            "content": text,
        })))
    }
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

