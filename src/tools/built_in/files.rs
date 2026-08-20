use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use regex::Regex;
use tokio::fs;

use crate::{
    message::{ToolDefinition, execution_files_dir},
    tools::{self, ExecutionContext, InternalTool, ToolResult},
};

mod edit;

#[derive(Clone)]
struct ScopedFiles {
    files_dir: PathBuf,
}

impl ScopedFiles {
    fn new(files_dir: PathBuf) -> Self {
        Self { files_dir }
    }

    async fn root(&self, ctx: &ExecutionContext) -> Result<PathBuf, String> {
        let root = execution_files_dir(&self.files_dir, ctx);
        fs::create_dir_all(&root)
            .await
            .map_err(|err| err.to_string())?;
        Ok(root)
    }

    async fn path(&self, ctx: &ExecutionContext, path: &str) -> Result<PathBuf, String> {
        let path = Path::new(path);
        if path.is_absolute()
            || path.components().any(|part| {
                matches!(
                    part,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err("path must be relative to this execution's workspace".to_string());
        }
        if path.as_os_str().is_empty() {
            return Err("path must not be empty".to_string());
        }

        let root = self.root(ctx).await?;
        let mut current = root.clone();
        for component in path.components() {
            let Component::Normal(name) = component else {
                continue;
            };
            current.push(name);
            if let Ok(metadata) = fs::symlink_metadata(&current).await
                && metadata.file_type().is_symlink()
            {
                return Err("symbolic links are not allowed in the workspace".to_string());
            }
        }
        Ok(root.join(path))
    }

    async fn read(
        &self,
        params: ReadFileParams,
        ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, String> {
        let content = fs::read_to_string(self.path(ctx, &params.path).await?)
            .await
            .map_err(|err| err.to_string())?;
        let lines = content.lines().collect::<Vec<_>>();
        let offset = params.offset.max(1);
        let limit = params.limit.clamp(1, 2_000);
        let start = offset.saturating_sub(1).min(lines.len());
        let end = start.saturating_add(limit).min(lines.len());
        let page = lines[start..end]
            .iter()
            .enumerate()
            .map(|(index, line)| format!("{}|{line}", start + index + 1))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(serde_json::json!({
            "content": page,
            "total_lines": lines.len(),
            "truncated": end < lines.len(),
            "next_offset": (end < lines.len()).then_some(end + 1),
        }))
    }

    async fn write(
        &self,
        params: WriteFileParams,
        ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, String> {
        let path = self.path(ctx, &params.path).await?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|err| err.to_string())?;
        }
        fs::write(path, params.content)
            .await
            .map_err(|err| err.to_string())?;
        Ok(serde_json::json!({}))
    }

    async fn edit(
        &self,
        params: EditFileParams,
        ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, String> {
        let path = self.path(ctx, &params.path).await?;
        let content = fs::read_to_string(&path)
            .await
            .map_err(|err| err.to_string())?;
        let result = edit::apply(&content, &params.path, &params.edits)?;
        fs::write(path, result.content)
            .await
            .map_err(|err| err.to_string())?;
        Ok(serde_json::json!({
            "edits": params.edits.len(),
            "normalized_match": result.normalized_match,
        }))
    }

    async fn search(
        &self,
        params: SearchFilesParams,
        ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, String> {
        let workspace = self.root(ctx).await?;
        let search_root = self.path(ctx, &params.path).await?;
        let files = collect_files(&search_root).await?;
        let limit = params.limit.clamp(1, 500);

        let results = match params.target {
            SearchTarget::Files => files
                .into_iter()
                .filter_map(|path| {
                    let relative = relative_path(&workspace, &path);
                    matches_file_glob(&params.pattern, &relative)
                        .then(|| serde_json::json!({ "path": relative }))
                })
                .collect(),
            SearchTarget::Content => {
                let pattern = Regex::new(&params.pattern)
                    .map_err(|err| format!("invalid search pattern: {err}"))?;
                let mut results = Vec::new();
                for path in files {
                    let relative = relative_path(&workspace, &path);
                    if params
                        .file_glob
                        .as_deref()
                        .is_some_and(|glob| !matches_file_glob(glob, &relative))
                    {
                        continue;
                    }
                    let Ok(content) = fs::read_to_string(&path).await else {
                        continue;
                    };
                    let lines = content.lines().collect::<Vec<_>>();
                    let matching = lines
                        .iter()
                        .enumerate()
                        .filter(|(_, line)| pattern.is_match(line))
                        .collect::<Vec<_>>();
                    match params.output_mode {
                        SearchOutput::Content => {
                            for (index, _) in matching {
                                let start = index.saturating_sub(params.context);
                                let end = (index + params.context + 1).min(lines.len());
                                let content = lines[start..end]
                                    .iter()
                                    .enumerate()
                                    .map(|(offset, line)| format!("{}|{line}", start + offset + 1))
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                results.push(serde_json::json!({
                                    "path": relative,
                                    "line": index + 1,
                                    "content": content,
                                }));
                            }
                        }
                        SearchOutput::FilesOnly if !matching.is_empty() => {
                            results.push(serde_json::json!({ "path": relative }));
                        }
                        SearchOutput::Count if !matching.is_empty() => {
                            results.push(serde_json::json!({
                                "path": relative,
                                "count": matching.len(),
                            }));
                        }
                        SearchOutput::FilesOnly | SearchOutput::Count => {}
                    }
                }
                results
            }
        };
        let total = results.len();
        let start = params.offset.min(total);
        let end = start.saturating_add(limit).min(total);
        Ok(serde_json::json!({
            "results": &results[start..end],
            "total_count": total,
            "truncated": end < total,
            "next_offset": (end < total).then_some(end),
        }))
    }
}

async fn collect_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    let metadata = fs::symlink_metadata(root)
        .await
        .map_err(|err| err.to_string())?;
    if metadata.is_file() {
        return Ok(vec![root.to_path_buf()]);
    }
    if !metadata.is_dir() {
        return Err("search path must be a file or directory".to_string());
    }

    let mut directories = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = directories.pop() {
        let mut entries = fs::read_dir(directory)
            .await
            .map_err(|err| err.to_string())?;
        while let Some(entry) = entries.next_entry().await.map_err(|err| err.to_string())? {
            let file_type = entry.file_type().await.map_err(|err| err.to_string())?;
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                directories.push(entry.path());
            } else if file_type.is_file() {
                files.push(entry.path());
            }
        }
    }
    files.sort();
    Ok(files)
}

fn relative_path(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn matches_file_glob(pattern: &str, path: &str) -> bool {
    glob_matches(pattern, path)
        || Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| glob_matches(pattern, name))
}

fn glob_matches(pattern: &str, path: &str) -> bool {
    let pattern = regex::escape(pattern)
        .replace(r"\*", ".*")
        .replace(r"\?", ".");
    Regex::new(&format!("^{pattern}$")).is_ok_and(|pattern| pattern.is_match(path))
}

macro_rules! file_tool {
    ($tool:ident, $params:ty, $name:literal, $description:literal, $method:ident) => {
        pub struct $tool(ScopedFiles);

        impl $tool {
            pub fn new(files_dir: PathBuf) -> Self {
                Self(ScopedFiles::new(files_dir))
            }
        }

        #[async_trait]
        impl InternalTool for $tool {
            fn name(&self) -> &str {
                $name
            }

            fn tool_definition(&self) -> ToolDefinition {
                tools::tool_definition::<$params>(self.name(), $description)
            }

            async fn execute(&self, args: serde_json::Value, ctx: &ExecutionContext) -> ToolResult {
                tools::execute_typed(args, |params: $params| self.0.$method(params, ctx)).await
            }
        }
    };
}

fn default_read_offset() -> usize {
    1
}

fn default_read_limit() -> usize {
    2_000
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ReadFileParams {
    /// File path relative to this execution's workspace.
    pub path: String,
    /// First line to return, starting at 1.
    #[serde(default = "default_read_offset")]
    pub offset: usize,
    /// Maximum number of lines to return.
    #[serde(default = "default_read_limit")]
    pub limit: usize,
}

file_tool!(
    ReadFileTool,
    ReadFileParams,
    "read_file",
    "Read a UTF-8 file with line numbers and pagination from this execution's private workspace.",
    read
);

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct WriteFileParams {
    /// File path relative to this execution's workspace.
    pub path: String,
    /// Complete file content. Existing content is replaced.
    pub content: String,
}

file_tool!(
    WriteFileTool,
    WriteFileParams,
    "write_file",
    "Write the complete contents of a UTF-8 file, creating it and its parent directories when needed. Existing content is overwritten.",
    write
);

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct EditFileParams {
    /// File path relative to this execution's workspace.
    pub path: String,
    /// Non-overlapping replacements, each matched against the original file.
    pub edits: Vec<FileEdit>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct FileEdit {
    /// Text to replace. It must identify one unique region of the original file.
    pub old_text: String,
    /// Replacement text. Use an empty string to delete the matched region.
    pub new_text: String,
}

file_tool!(
    EditFileTool,
    EditFileParams,
    "edit",
    "Make one or more targeted replacements in a UTF-8 file. Every old_text must identify a unique, non-overlapping region of the original file. Exact matching is tried before conservative whitespace and Unicode normalization.",
    edit
);

#[derive(Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SearchTarget {
    #[default]
    Content,
    Files,
}

#[derive(Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SearchOutput {
    #[default]
    Content,
    FilesOnly,
    Count,
}

fn default_search_path() -> String {
    ".".to_string()
}

fn default_search_limit() -> usize {
    50
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct SearchFilesParams {
    /// Regex for content search or * and ? pattern for filename search.
    pub pattern: String,
    #[serde(default)]
    pub target: SearchTarget,
    /// File or directory relative to this execution's workspace.
    #[serde(default = "default_search_path")]
    pub path: String,
    /// Optional * and ? filename filter for content search.
    #[serde(default)]
    pub file_glob: Option<String>,
    #[serde(default = "default_search_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub output_mode: SearchOutput,
    /// Context lines before and after each content match.
    #[serde(default)]
    pub context: usize,
}

file_tool!(
    SearchFilesTool,
    SearchFilesParams,
    "search_files",
    "Search UTF-8 file contents with a regex or find files by name inside this execution's private workspace. Results are paginated.",
    search
);

#[cfg(test)]
mod tests {
    use super::*;

    fn test_files(name: &str) -> (PathBuf, ExecutionContext) {
        let root =
            std::env::temp_dir().join(format!("ironpaw-file-tool-test-{}", uuid::Uuid::new_v4()));
        let ctx = ExecutionContext::new(crate::message::automation_workspace(name));
        (root, ctx)
    }

    #[tokio::test]
    async fn paths_cannot_escape_the_scope() {
        let files = ScopedFiles::new(std::env::temp_dir());
        let ctx = ExecutionContext::new(crate::message::automation_workspace("morning"));
        assert!(files.path(&ctx, "../secrets").await.is_err());
        assert!(files.path(&ctx, "/etc/passwd").await.is_err());
    }

    #[tokio::test]
    async fn write_read_and_edit_stay_in_the_automation_workspace() {
        let (root, ctx) = test_files("morning");
        let files = ScopedFiles::new(root.clone());
        let edit = EditFileTool::new(root.clone());
        files
            .write(
                WriteFileParams {
                    path: "state/today.txt".to_string(),
                    content: "first\nsecond\nthird".to_string(),
                },
                &ctx,
            )
            .await
            .unwrap();
        edit.execute(
            serde_json::json!({
                "path": "state/today.txt",
                "edits": [{ "old_text": "second", "new_text": "changed" }],
            }),
            &ctx,
        )
        .await
        .unwrap();
        let read = files
            .read(
                ReadFileParams {
                    path: "state/today.txt".to_string(),
                    offset: 2,
                    limit: 1,
                },
                &ctx,
            )
            .await
            .unwrap();

        assert_eq!(read["content"], "2|changed");
        assert_eq!(read["next_offset"], 3);
        assert!(!root.join("user/state/today.txt").exists());
    }

    #[tokio::test]
    async fn search_finds_content_and_files() {
        let (root, ctx) = test_files("search");
        let files = ScopedFiles::new(root);
        for (path, content) in [("notes/one.txt", "alpha"), ("notes/two.md", "beta alpha")] {
            files
                .write(
                    WriteFileParams {
                        path: path.to_string(),
                        content: content.to_string(),
                    },
                    &ctx,
                )
                .await
                .unwrap();
        }

        let content = files
            .search(
                SearchFilesParams {
                    pattern: "alpha".to_string(),
                    target: SearchTarget::Content,
                    path: ".".to_string(),
                    file_glob: Some("*.txt".to_string()),
                    limit: 50,
                    offset: 0,
                    output_mode: SearchOutput::Content,
                    context: 0,
                },
                &ctx,
            )
            .await
            .unwrap();
        let names = files
            .search(
                SearchFilesParams {
                    pattern: "*.md".to_string(),
                    target: SearchTarget::Files,
                    path: ".".to_string(),
                    file_glob: None,
                    limit: 50,
                    offset: 0,
                    output_mode: SearchOutput::Content,
                    context: 0,
                },
                &ctx,
            )
            .await
            .unwrap();

        assert_eq!(content["total_count"], 1);
        assert_eq!(names["results"][0]["path"], "notes/two.md");
    }
}
