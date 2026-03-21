use std::path::PathBuf;

use async_trait::async_trait;

use crate::core::ToolDefinition;
use crate::tools::Tool;

const MAX_NAME_LEN: usize = 64;
const MAX_PROMPT_SIZE: usize = 51200; // 50 KB

pub struct CreateSubAgentTool {
    subagents_dir: PathBuf,
}

impl CreateSubAgentTool {
    pub fn new(memory_dir: PathBuf) -> Self {
        Self {
            subagents_dir: memory_dir.join("subagents"),
        }
    }
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Agent name cannot be empty.".to_string());
    }

    if name.len() > MAX_NAME_LEN {
        return Err(format!(
            "Agent name too long: {} chars (max {MAX_NAME_LEN}).",
            name.len()
        ));
    }

    if name.starts_with('-') {
        return Err("Agent name cannot start with '-'.".to_string());
    }

    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err("Invalid agent name: path traversal is not allowed.".to_string());
    }

    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(
            "Invalid agent name: only ASCII alphanumeric characters, '_' and '-' are allowed."
                .to_string(),
        );
    }

    Ok(())
}

#[async_trait]
impl Tool for CreateSubAgentTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "create_sub_agent".to_string(),
            description: "Create or update a sub-agent by writing its complete system prompt to \
                memory/subagents/{name}.md. Unlike write_memory (append-only), this OVERWRITES \
                the file entirely — the prompt must be self-contained and complete. \
                After creation, the agent becomes available for spawn_sub_agent."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Agent name (ASCII alphanumeric, '_', '-'). Used as filename without .md extension."
                    },
                    "system_prompt": {
                        "type": "string",
                        "description": "Complete self-contained system prompt for the sub-agent."
                    }
                },
                "required": ["name", "system_prompt"]
            }),
        }
    }

    async fn execute(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid arguments: {e}"))?;

        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required parameter: name".to_string())?
            .trim();

        let system_prompt = args
            .get("system_prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required parameter: system_prompt".to_string())?;

        validate_name(name)?;

        if system_prompt.trim().is_empty() {
            return Err("System prompt cannot be empty.".to_string());
        }

        if system_prompt.len() > MAX_PROMPT_SIZE {
            return Err(format!(
                "System prompt too large: {} bytes (max {MAX_PROMPT_SIZE}).",
                system_prompt.len()
            ));
        }

        tokio::fs::create_dir_all(&self.subagents_dir)
            .await
            .map_err(|e| format!("Failed to create subagents directory: {e}"))?;

        let path = self.subagents_dir.join(format!("{name}.md"));
        let existed_before = path
            .try_exists()
            .map_err(|e| format!("Failed to check existing sub-agent file: {e}"))?;

        tokio::fs::write(&path, system_prompt)
            .await
            .map_err(|e| format!("Failed to write sub-agent prompt: {e}"))?;

        let action = if existed_before { "Updated" } else { "Created" };
        let mut result = format!(
            "{action} sub-agent '{name}' at {}. It is now available for spawn_sub_agent.",
            path.display()
        );

        if !system_prompt.lines().any(|l| l.trim_start().starts_with('#')) {
            result.push_str(
                "\n⚠️ Warning: system prompt has no markdown heading (#). Consider adding one for clarity."
            );
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_name_valid() {
        assert!(validate_name("my-agent").is_ok());
        assert!(validate_name("agent_v2").is_ok());
        assert!(validate_name("a").is_ok());
    }

    #[test]
    fn validate_name_empty() {
        assert!(validate_name("").is_err());
    }

    #[test]
    fn validate_name_path_traversal() {
        assert!(validate_name("..").is_err());
        assert!(validate_name("../etc").is_err());
        assert!(validate_name("foo/bar").is_err());
        assert!(validate_name("foo\\bar").is_err());
    }

    #[test]
    fn validate_name_invalid_chars() {
        assert!(validate_name("hello world").is_err());
        assert!(validate_name("agent@home").is_err());
        assert!(validate_name("name.ext").is_err());
    }

    #[test]
    fn validate_name_too_long() {
        let long_name = "a".repeat(MAX_NAME_LEN + 1);
        assert!(validate_name(&long_name).is_err());

        let ok_name = "a".repeat(MAX_NAME_LEN);
        assert!(validate_name(&ok_name).is_ok());
    }

    #[test]
    fn validate_name_cannot_start_with_dash() {
        assert!(validate_name("-agent").is_err());
    }

    #[tokio::test]
    async fn execute_rejects_oversized_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let tool = CreateSubAgentTool::new(dir.path().to_path_buf());
        let big_prompt = "x".repeat(MAX_PROMPT_SIZE + 1);
        let args = serde_json::json!({
            "name": "test",
            "system_prompt": big_prompt,
        });

        let result = tool.execute(&args.to_string()).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("too large"));
    }

    #[tokio::test]
    async fn execute_rejects_blank_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let tool = CreateSubAgentTool::new(dir.path().to_path_buf());
        let args = serde_json::json!({
            "name": "test",
            "system_prompt": "   \n\t  ",
        });

        let result = tool.execute(&args.to_string()).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("cannot be empty"));
    }

    #[tokio::test]
    async fn execute_warns_on_missing_heading() {
        let dir = tempfile::tempdir().unwrap();
        let tool = CreateSubAgentTool::new(dir.path().to_path_buf());
        let args = serde_json::json!({
            "name": "test",
            "system_prompt": "No heading here, just text.",
        });

        let result = tool.execute(&args.to_string()).await.unwrap();
        assert!(result.contains("Warning"));
    }

    #[tokio::test]
    async fn execute_no_warning_with_heading() {
        let dir = tempfile::tempdir().unwrap();
        let tool = CreateSubAgentTool::new(dir.path().to_path_buf());
        let args = serde_json::json!({
            "name": "test",
            "system_prompt": "# My Agent\nDoes things.",
        });

        let result = tool.execute(&args.to_string()).await.unwrap();
        assert!(!result.contains("Warning"));
    }

    #[tokio::test]
    async fn execute_creates_and_updates_file() {
        let dir = tempfile::tempdir().unwrap();
        let tool = CreateSubAgentTool::new(dir.path().to_path_buf());

        let create_args = serde_json::json!({
            "name": "test",
            "system_prompt": "# First\nalpha",
        });
        let create_result = tool.execute(&create_args.to_string()).await.unwrap();
        assert!(create_result.contains("Created"));

        let path = dir.path().join("subagents").join("test.md");
        let first = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(first, "# First\nalpha");

        let update_args = serde_json::json!({
            "name": "test",
            "system_prompt": "# Second\nbeta",
        });
        let update_result = tool.execute(&update_args.to_string()).await.unwrap();
        assert!(update_result.contains("Updated"));

        let second = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(second, "# Second\nbeta");
    }

    #[tokio::test]
    async fn execute_rejects_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let tool = CreateSubAgentTool::new(dir.path().to_path_buf());

        let result = tool.execute("{not-json").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid arguments"));
    }

    #[tokio::test]
    async fn execute_creates_subagents_dir() {
        let dir = tempfile::tempdir().unwrap();
        let tool = CreateSubAgentTool::new(dir.path().to_path_buf());

        let args = serde_json::json!({
            "name": "test",
            "system_prompt": "# Agent\nHello",
        });

        let _ = tool.execute(&args.to_string()).await.unwrap();
        assert!(dir.path().join("subagents").is_dir());
    }
}
