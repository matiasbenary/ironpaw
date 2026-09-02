use std::path::PathBuf;

use async_trait::async_trait;

use crate::{
    message::{available_filename, execution_files_dir},
    network::public::PublicHttp,
    tools::{self, ExecutionContext, InternalTool, ToolResult},
};

const MAX_DOWNLOAD_BYTES: usize = 10 * 1024 * 1024;

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct DownloadFileParams {
    /// Public HTTPS URL to download.
    pub url: String,
}

#[derive(serde::Serialize)]
struct DownloadedFile {
    filename: String,
    mime_type: String,
    size_bytes: usize,
}

pub struct DownloadFileTool {
    files_dir: PathBuf,
    http: PublicHttp,
}

impl DownloadFileTool {
    pub fn new(files_dir: PathBuf) -> Self {
        Self {
            files_dir,
            http: PublicHttp::new(),
        }
    }

    async fn download(
        &self,
        params: DownloadFileParams,
        ctx: &ExecutionContext,
    ) -> Result<DownloadedFile, String> {
        let url = reqwest::Url::parse(&params.url).map_err(|err| format!("invalid url: {err}"))?;
        let url = normalize_download_url(url)?;
        let response = self
            .http
            .get(url.as_str(), MAX_DOWNLOAD_BYTES, true)
            .await
            .map_err(|error| {
                error.replace(
                    "response is too large (> 10485760 bytes)",
                    "download is too large (> 10 MB)",
                )
            })?;
        if !(200..300).contains(&response.status) {
            return Err(format!("request failed with status {}", response.status));
        }
        let mime_type = response.content_type;
        let bytes = response.body;

        let requested_filename = url
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .filter(|name| !name.is_empty())
            .unwrap_or("download")
            .to_string();
        let extension = extension_for(&requested_filename, &mime_type);
        let directory = execution_files_dir(&self.files_dir, ctx);
        std::fs::create_dir_all(&directory).map_err(|err| err.to_string())?;
        let filename = available_filename(
            &directory,
            Some(&requested_filename),
            "download",
            &extension,
        );
        std::fs::write(directory.join(&filename), &bytes).map_err(|err| err.to_string())?;

        Ok(DownloadedFile {
            filename,
            mime_type,
            size_bytes: bytes.len(),
        })
    }
}

#[async_trait]
impl InternalTool for DownloadFileTool {
    tools::tool_metadata!(
        "download_file",
        DownloadFileParams,
        "Download a public URL into Ironpaw's managed download directory and return its filename. Public Google Sheets, Docs, and Slides editor links are exported as CSV, text, and PDF. Use inspect_file with the filename to query contents without putting the whole download into conversation history."
    );

    async fn execute(&self, args: serde_json::Value, ctx: &ExecutionContext) -> ToolResult {
        tools::execute_typed(args, |params| self.download(params, ctx)).await
    }
}

fn extension_for(filename: &str, mime_type: &str) -> String {
    std::path::Path::new(filename)
        .extension()
        .and_then(|extension| extension.to_str())
        .filter(|extension| {
            !extension.is_empty()
                && extension.len() <= 10
                && extension.chars().all(|c| c.is_ascii_alphanumeric())
        })
        .unwrap_or_else(|| extension_for_mime(mime_type))
        .to_ascii_lowercase()
}

fn extension_for_mime(mime_type: &str) -> &'static str {
    match mime_type {
        "text/csv" => "csv",
        "text/plain" => "txt",
        "application/json" => "json",
        "application/pdf" => "pdf",
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        _ => "bin",
    }
}

fn normalize_download_url(mut url: reqwest::Url) -> Result<reqwest::Url, String> {
    if url.host_str() != Some("docs.google.com") {
        return Ok(url);
    }
    let segments = url
        .path_segments()
        .map(|segments| segments.collect::<Vec<_>>())
        .unwrap_or_default();
    let Some(document_id) = segments.get(2).filter(|id| !id.is_empty()) else {
        return Ok(url);
    };
    let kind = segments.first().copied().unwrap_or_default();
    let document_id = document_id.to_string();
    match kind {
        "spreadsheets" => {
            let gid = url
                .query_pairs()
                .find(|(key, _)| key == "gid")
                .map(|(_, value)| value.into_owned())
                .or_else(|| fragment_parameter(url.fragment(), "gid"));
            url.set_path(&format!("/spreadsheets/d/{document_id}/gviz/tq"));
            url.set_query(None);
            url.set_fragment(None);
            url.query_pairs_mut().append_pair("tqx", "out:csv");
            if let Some(gid) = gid {
                url.query_pairs_mut().append_pair("gid", &gid);
            }
        }
        "document" => {
            url.set_path(&format!("/document/d/{document_id}/export"));
            url.set_query(Some("format=txt"));
            url.set_fragment(None);
        }
        "presentation" => {
            url.set_path(&format!("/presentation/d/{document_id}/export/pdf"));
            url.set_query(None);
            url.set_fragment(None);
        }
        _ => {}
    }
    Ok(url)
}

fn fragment_parameter(fragment: Option<&str>, name: &str) -> Option<String> {
    fragment?.split('&').find_map(|part| {
        let (key, value) = part.split_once('=')?;
        (key == name).then(|| value.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "local-development")]
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[cfg(feature = "local-development")]
    #[tokio::test]
    async fn downloads_file_into_download_directory() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/albums.csv", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 1024];
            let _ = socket.read(&mut buffer).await;
            socket.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/csv\r\ncontent-length: 17\r\nconnection: close\r\n\r\nartist,album\nA,B\n").await.unwrap();
        });
        let dir =
            std::env::temp_dir().join(format!("ironpaw-download-test-{}", uuid::Uuid::new_v4()));
        let downloaded = DownloadFileTool::new(dir.clone())
            .download(
                DownloadFileParams { url },
                &ExecutionContext::new(crate::message::automation_workspace("download-test")),
            )
            .await
            .unwrap();
        assert_eq!(downloaded.mime_type, "text/csv");
        assert_eq!(downloaded.filename, "albums.csv");
        assert!(
            dir.join("automations/646f776e6c6f61642d74657374/albums.csv")
                .exists()
        );
    }

    #[test]
    fn normalizes_public_google_workspace_urls() {
        let sheet = normalize_download_url(
            reqwest::Url::parse(
                "https://docs.google.com/spreadsheets/d/sheet-id/edit?usp=sharing#gid=42",
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            sheet.as_str(),
            "https://docs.google.com/spreadsheets/d/sheet-id/gviz/tq?tqx=out%3Acsv&gid=42"
        );

        let document = normalize_download_url(
            reqwest::Url::parse("https://docs.google.com/document/d/doc-id/edit").unwrap(),
        )
        .unwrap();
        assert_eq!(
            document.as_str(),
            "https://docs.google.com/document/d/doc-id/export?format=txt"
        );

        let slides = normalize_download_url(
            reqwest::Url::parse("https://docs.google.com/presentation/d/slides-id/edit").unwrap(),
        )
        .unwrap();
        assert_eq!(
            slides.as_str(),
            "https://docs.google.com/presentation/d/slides-id/export/pdf"
        );
    }
}
