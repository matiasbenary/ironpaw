use std::{path::PathBuf, time::Duration};

use async_trait::async_trait;

use crate::{
    message::ToolDefinition,
    tools::{self, ExecutionContext, InternalTool, ToolResponder},
};

use super::web::fetch::{guard_url, pinned_client};

const MAX_DOWNLOAD_BYTES: u64 = 10 * 1024 * 1024;

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct DownloadFileParams {
    /// Public HTTPS URL to download. HTTP is allowed only for localhost.
    pub url: String,
}

#[derive(serde::Serialize)]
struct DownloadedFile {
    file_id: u32,
    filename: String,
    mime_type: String,
    size_bytes: usize,
}

pub struct DownloadFileTool {
    download_dir: PathBuf,
    client: reqwest::Client,
}

impl DownloadFileTool {
    pub fn new(download_dir: PathBuf) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("http client should build");
        Self {
            download_dir,
            client,
        }
    }

    async fn download(&self, params: DownloadFileParams) -> Result<DownloadedFile, String> {
        let url = reqwest::Url::parse(&params.url).map_err(|err| format!("invalid url: {err}"))?;
        let url = normalize_download_url(url)?;
        let resolved = guard_url(&url).await?;
        let client = match resolved {
            Some(addrs) => pinned_client(url.host_str().unwrap_or_default(), &addrs)?,
            None => self.client.clone(),
        };
        let mut response = client
            .get(url.clone())
            .send()
            .await
            .map_err(|err| format!("request failed: {}", err.without_url()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("request failed with status {}", status.as_u16()));
        }
        if response
            .content_length()
            .is_some_and(|size| size > MAX_DOWNLOAD_BYTES)
        {
            return Err("download is too large (> 10 MB)".to_string());
        }
        let mime_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("application/octet-stream")
            .split(';')
            .next()
            .unwrap_or("application/octet-stream")
            .trim()
            .to_string();
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|err| format!("failed to read body: {}", err.without_url()))?
        {
            if bytes.len() as u64 + chunk.len() as u64 > MAX_DOWNLOAD_BYTES {
                return Err("download is too large (> 10 MB)".to_string());
            }
            bytes.extend_from_slice(&chunk);
        }

        let filename = url
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .filter(|name| !name.is_empty())
            .unwrap_or("download")
            .to_string();
        let extension = extension_for(&filename, &mime_type);
        let file_id = random_file_id();
        std::fs::create_dir_all(&self.download_dir).map_err(|err| err.to_string())?;
        std::fs::write(
            self.download_dir.join(format!("{file_id}.{extension}")),
            &bytes,
        )
        .map_err(|err| err.to_string())?;

        Ok(DownloadedFile {
            file_id,
            filename,
            mime_type,
            size_bytes: bytes.len(),
        })
    }
}

#[async_trait]
impl InternalTool for DownloadFileTool {
    fn name(&self) -> &str {
        "download_file"
    }

    fn tool_definition(&self) -> ToolDefinition {
        tools::tool_definition::<DownloadFileParams>(
            self.name(),
            "Download a public URL into Ironpaw's managed download directory and return a file_id. Public Google Sheets, Docs, and Slides editor links are exported as CSV, text, and PDF. Use inspect_file with the file_id to query contents without putting the whole download into conversation history.",
        )
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ExecutionContext,
        responder: ToolResponder,
    ) {
        responder.respond(tools::execute_typed(args, |params| self.download(params)).await);
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

fn random_file_id() -> u32 {
    let bytes = *uuid::Uuid::new_v4().as_bytes();
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
            .download(DownloadFileParams { url })
            .await
            .unwrap();
        assert_eq!(downloaded.mime_type, "text/csv");
        assert!(dir.join(format!("{}.csv", downloaded.file_id)).exists());
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
