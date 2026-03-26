use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;

use crate::core::ToolDefinition;
use crate::git_sync::GitSync;
use crate::tools::Tool;

pub struct ReadMemoryTool {
    memory_dir: PathBuf,
}

impl ReadMemoryTool {
    pub fn new(memory_dir: PathBuf) -> Self {
        Self { memory_dir }
    }
}

#[async_trait]
impl Tool for ReadMemoryTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read_memory".to_string(),
            description: "Read agent's long-term memory files. Without filename — returns list of available .md files (including subdirectories like subagents/). With filename — returns file contents.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "filename": {
                        "type": "string",
                        "description": "Path to the .md file to read, e.g. 'AGENT.md' or 'subagents/code_reviewer.md'. If omitted, returns a list of all available memory files."
                    }
                },
                "required": []
            }),
        }
    }

    async fn execute(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid arguments: {e}"))?;

        let filename = args
            .get("filename")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();

        if filename.is_empty() {
            self.list_files().await
        } else {
            self.read_file(filename).await
        }
    }
}

impl ReadMemoryTool {
    async fn list_files(&self) -> Result<String, String> {
        let mut files = Vec::new();
        Self::collect_md_files(&self.memory_dir, &self.memory_dir, &mut files)
            .map_err(|e| format!("Cannot read memory directory: {e}"))?;

        files.sort();

        if files.is_empty() {
            Ok("No memory files found.".to_string())
        } else {
            Ok(files.join("\n"))
        }
    }

    fn collect_md_files(
        base: &std::path::Path,
        dir: &std::path::Path,
        files: &mut Vec<String>,
    ) -> Result<(), std::io::Error> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                let dir_name = entry.file_name().to_string_lossy().to_string();
                // Skip hidden dirs and non-memory dirs
                if !dir_name.starts_with('.') && dir_name != "evals" {
                    Self::collect_md_files(base, &path, files)?;
                }
            } else {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".md") {
                    let relative = path
                        .strip_prefix(base)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .to_string();
                    files.push(relative);
                }
            }
        }
        Ok(())
    }

    async fn read_file(&self, filename: &str) -> Result<String, String> {
        validate_filename(filename)?;

        let path = self.memory_dir.join(filename);
        tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| format!("Cannot read file '{filename}': {e}"))
    }
}

fn validate_filename(filename: &str) -> Result<(), String> {
    if filename.is_empty() {
        return Err("Filename is required.".to_string());
    }
    if filename.contains("..") || filename.contains('\\') {
        return Err("Invalid filename: path traversal is not allowed.".to_string());
    }
    if filename.starts_with('/') {
        return Err("Invalid filename: absolute paths are not allowed.".to_string());
    }
    if !filename.ends_with(".md") {
        return Err("Only .md files are allowed.".to_string());
    }
    Ok(())
}

pub struct WriteMemoryTool {
    memory_dir: PathBuf,
    git_sync: Option<Arc<GitSync>>,
}

impl WriteMemoryTool {
    pub fn new(memory_dir: PathBuf) -> Self {
        Self {
            memory_dir,
            git_sync: None,
        }
    }

    pub fn with_git_sync(mut self, git_sync: Arc<GitSync>) -> Self {
        self.git_sync = Some(git_sync);
        self
    }
}

#[async_trait]
impl Tool for WriteMemoryTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write_memory".to_string(),
            description: "Write to agent's long-term memory. Creates a new .md file or appends to an existing one. Supports subdirectories.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "filename": {
                        "type": "string",
                        "description": "Path to the .md file to write to (e.g. 'user_preferences.md' or 'subagents/code_reviewer.md'). Must end with .md."
                    },
                    "content": {
                        "type": "string",
                        "description": "Text content to write. Will be appended if the file already exists."
                    }
                },
                "required": ["filename", "content"]
            }),
        }
    }

    fn to_intention(&self, arguments: &str) -> Option<crate::esaa::Intention> {
        let args: serde_json::Value = serde_json::from_str(arguments).ok()?;
        let key = args.get("filename")?.as_str()?.to_string();
        let content = args.get("content")?.as_str()?.to_string();
        Some(crate::esaa::Intention::WriteMemory { key, content })
    }

    async fn execute(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid arguments: {e}"))?;

        let filename = args
            .get("filename")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();

        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        validate_filename(filename)?;

        if content.is_empty() {
            return Err("Content cannot be empty.".to_string());
        }

        let path = self.memory_dir.join(filename);

        // Create parent directories if needed (e.g. subagents/)
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("Cannot create directory: {e}"))?;
        }

        let exists = path.exists();

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|e| format!("Cannot open file '{filename}': {e}"))?;

        // Add a newline separator when appending to existing content
        if exists {
            let metadata = tokio::fs::metadata(&path)
                .await
                .map_err(|e| format!("Cannot read file metadata: {e}"))?;
            if metadata.len() > 0 {
                file.write_all(b"\n")
                    .await
                    .map_err(|e| format!("Cannot write to file: {e}"))?;
            }
        }

        file.write_all(content.as_bytes())
            .await
            .map_err(|e| format!("Cannot write to file: {e}"))?;

        let msg = if exists {
            format!("Appended to '{filename}'.")
        } else {
            format!("Created '{filename}'.")
        };

        // Auto-commit if git sync is enabled
        if let Some(ref gs) = self.git_sync {
            let commit_msg = if exists {
                format!("update: {filename}")
            } else {
                format!("create: {filename}")
            };
            gs.commit(&commit_msg);
        }

        Ok(msg)
    }
}
