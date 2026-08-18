use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;

use crate::{
    message::{ToolDefinition, execution_files_dir},
    tools::{self, ExecutionContext, InternalTool, ToolResult},
};

/// File tools are the only filesystem surface exposed to the agent. Their
/// root is selected from the host-owned execution context.
#[derive(Clone)]
struct ScopedFiles {
    files_dir: PathBuf,
}

impl ScopedFiles {
    fn new(files_dir: PathBuf) -> Self {
        Self { files_dir }
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

        let root = execution_files_dir(&self.files_dir, ctx);
        fs::create_dir_all(&root)
            .await
            .map_err(|err| err.to_string())?;

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

    async fn list(
        &self,
        params: ListFilesParams,
        ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, String> {
        let path = self
            .path(ctx, params.path.as_deref().unwrap_or("."))
            .await?;
        let mut directory = fs::read_dir(path).await.map_err(|err| err.to_string())?;
        let mut entries = Vec::new();
        while let Some(entry) = directory
            .next_entry()
            .await
            .map_err(|err| err.to_string())?
        {
            entries.push(serde_json::json!({
                "name": entry.file_name().to_string_lossy(),
                "kind": if entry.file_type().await.map_err(|err| err.to_string())?.is_dir() {
                    "directory"
                } else {
                    "file"
                },
            }));
        }
        entries.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
        Ok(serde_json::json!({ "entries": entries }))
    }

    async fn read(
        &self,
        params: ReadFileParams,
        ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, String> {
        let content = fs::read_to_string(self.path(ctx, &params.path).await?)
            .await
            .map_err(|err| err.to_string())?;
        Ok(serde_json::json!({ "content": content }))
    }

    async fn create(
        &self,
        params: CreateFileParams,
        ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, String> {
        let path = self.path(ctx, &params.path).await?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|err| err.to_string())?;
        }
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .await
            .map_err(|err| err.to_string())?
            .write_all(params.content.as_bytes())
            .await
            .map_err(|err| err.to_string())?;
        Ok(serde_json::json!({}))
    }

    async fn update(
        &self,
        params: UpdateFileParams,
        ctx: &ExecutionContext,
    ) -> Result<serde_json::Value, String> {
        if params.old_content.is_empty() {
            return Err("old_content must not be empty".to_string());
        }
        let path = self.path(ctx, &params.path).await?;
        let content = fs::read_to_string(&path)
            .await
            .map_err(|err| err.to_string())?;
        if content.matches(&params.old_content).count() != 1 {
            return Err("old_content must occur exactly once in the file".to_string());
        }
        fs::write(
            path,
            content.replacen(&params.old_content, &params.new_content, 1),
        )
        .await
        .map_err(|err| err.to_string())?;
        Ok(serde_json::json!({}))
    }
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

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ListFilesParams {
    /// Directory relative to this execution's workspace. Defaults to the workspace root.
    #[serde(default)]
    pub path: Option<String>,
}

file_tool!(
    ListFilesTool,
    ListFilesParams,
    "list_files",
    "List files in this execution's private workspace. Paths are relative to that workspace.",
    list
);

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct ReadFileParams {
    /// File path relative to this execution's workspace.
    pub path: String,
}

file_tool!(
    ReadFileTool,
    ReadFileParams,
    "read_file",
    "Read a UTF-8 file from this execution's private workspace.",
    read
);

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct CreateFileParams {
    /// File path relative to this execution's workspace.
    pub path: String,
    pub content: String,
}

file_tool!(
    CreateFileTool,
    CreateFileParams,
    "create_file",
    "Create a new UTF-8 file in this execution's private workspace. Fails if it exists.",
    create
);

#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct UpdateFileParams {
    /// File path relative to this execution's workspace.
    pub path: String,
    /// Exact text to replace. It must occur once in the file.
    pub old_content: String,
    /// Replacement text.
    pub new_content: String,
}

file_tool!(
    UpdateFileTool,
    UpdateFileParams,
    "update_file",
    "Make one targeted edit to an existing UTF-8 file. Read the file first, then provide its exact old_content and replacement new_content.",
    update
);

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn paths_cannot_escape_the_scope() {
        let files = ScopedFiles::new(std::env::temp_dir());
        let ctx = ExecutionContext::new(crate::message::automation_workspace("morning"));
        assert!(files.path(&ctx, "../secrets").await.is_err());
        assert!(files.path(&ctx, "/etc/passwd").await.is_err());
    }

    #[tokio::test]
    async fn create_and_update_stay_in_the_automation_workspace() {
        let root =
            std::env::temp_dir().join(format!("ironpaw-file-tool-test-{}", uuid::Uuid::new_v4()));
        let create = CreateFileTool::new(root.clone());
        let update = UpdateFileTool::new(root.clone());
        let ctx = ExecutionContext::new(crate::message::automation_workspace("morning"));

        create
            .execute(
                serde_json::json!({ "path": "state/today.txt", "content": "first" }),
                &ctx,
            )
            .await
            .unwrap();
        update
            .execute(
                serde_json::json!({
                    "path": "state/today.txt",
                    "old_content": "first",
                    "new_content": "second",
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert_eq!(
            fs::read_to_string(root.join("automations/6d6f726e696e67/state/today.txt"))
                .await
                .unwrap(),
            "second"
        );
        assert!(!root.join("user/state/today.txt").exists());
    }
}
