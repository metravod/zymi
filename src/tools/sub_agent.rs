use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use crate::core::agent::Agent;
use crate::core::approval::{ContextualApprovalHandler, SharedApprovalHandler};
use crate::core::{LlmProvider, ToolDefinition};
use crate::sandbox::{ExecutionContext, SandboxManager};
use crate::storage::in_memory::InMemoryStorage;
use crate::tools::current_time::CurrentTimeTool;
use crate::tools::memory::{ReadMemoryTool, WriteMemoryTool};
use crate::tools::shell::ShellTool;
use crate::tools::web_scrape::WebScrapeTool;
use crate::tools::web_search::WebSearchTool;
use crate::tools::Tool;

const MAX_SPAWN_DEPTH: u8 = 3;

pub struct SpawnSubAgentTool {
    provider: Arc<dyn LlmProvider>,
    memory_dir: PathBuf,
    subagents_dir: PathBuf,
    approval_handler: SharedApprovalHandler,
    current_depth: u8,
    sandbox: Option<Arc<SandboxManager>>,
}

impl SpawnSubAgentTool {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        memory_dir: PathBuf,
        approval_handler: SharedApprovalHandler,
    ) -> Self {
        Self::with_depth(provider, memory_dir, approval_handler, 0)
    }

    pub fn with_depth(
        provider: Arc<dyn LlmProvider>,
        memory_dir: PathBuf,
        approval_handler: SharedApprovalHandler,
        depth: u8,
    ) -> Self {
        let subagents_dir = memory_dir.join("subagents");
        Self {
            provider,
            memory_dir,
            subagents_dir,
            approval_handler,
            current_depth: depth,
            sandbox: None,
        }
    }

    pub fn with_sandbox(mut self, sandbox: Arc<SandboxManager>) -> Self {
        self.sandbox = Some(sandbox);
        self
    }

    fn list_available_agents(&self) -> Vec<String> {
        let entries = match std::fs::read_dir(&self.subagents_dir) {
            Ok(entries) => entries,
            Err(_) => return vec![],
        };

        let mut agents: Vec<String> = entries
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".md") {
                    Some(name.trim_end_matches(".md").to_string())
                } else {
                    None
                }
            })
            .collect();

        agents.sort();
        agents
    }
}

fn validate_agent_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Agent name is required.".to_string());
    }
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err("Invalid agent name: path traversal is not allowed.".to_string());
    }
    Ok(())
}

#[async_trait]
impl Tool for SpawnSubAgentTool {
    fn definition(&self) -> ToolDefinition {
        let agents = self.list_available_agents();
        let agents_list = if agents.is_empty() {
            "No sub-agents available. Create .md files in memory/subagents/ to add them.".to_string()
        } else {
            format!("Available agents: {}", agents.join(", "))
        };

        ToolDefinition {
            name: "spawn_sub_agent".to_string(),
            description: format!(
                "Delegate a task to a specialized sub-agent. The sub-agent runs in an isolated \
                context with its own conversation history. It has access to: get_current_time, \
                read_memory, write_memory, execute_shell (with user approval), web_search \
                (if configured), web_scrape (if configured). It cannot spawn other sub-agents. {}",
                agents_list
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_name": {
                        "type": "string",
                        "description": "Name of the sub-agent to spawn (without .md extension)"
                    },
                    "task": {
                        "type": "string",
                        "description": "The task description to send to the sub-agent"
                    }
                },
                "required": ["agent_name", "task"]
            }),
        }
    }

    async fn execute(&self, arguments: &str) -> Result<String, String> {
        if self.current_depth >= MAX_SPAWN_DEPTH {
            return Err(format!(
                "Sub-agent spawn depth limit ({MAX_SPAWN_DEPTH}) reached. Cannot spawn deeper."
            ));
        }

        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid arguments: {e}"))?;

        let agent_name = args
            .get("agent_name")
            .and_then(|v| v.as_str())
            .ok_or("Missing required parameter: agent_name")?
            .trim();

        let task = args
            .get("task")
            .and_then(|v| v.as_str())
            .ok_or("Missing required parameter: task")?;

        validate_agent_name(agent_name)?;

        let prompt_path = self.subagents_dir.join(format!("{agent_name}.md"));
        let system_prompt = tokio::fs::read_to_string(&prompt_path)
            .await
            .map_err(|e| format!("Cannot read sub-agent '{agent_name}': {e}"))?;

        let shell = if let Some(ref sb) = self.sandbox {
            ShellTool::new().with_sandbox(sb.clone(), ExecutionContext::SubAgent)
        } else {
            ShellTool::new()
        };
        let mut tools: Vec<Box<dyn Tool>> = vec![
            Box::new(CurrentTimeTool),
            Box::new(ReadMemoryTool::new(self.memory_dir.clone())),
            Box::new(WriteMemoryTool::new(self.memory_dir.clone())),
            Box::new(shell),
        ];

        if let Some(tool) = WebSearchTool::new() {
            tools.push(Box::new(tool));
        }
        if let Some(tool) = WebScrapeTool::new() {
            tools.push(Box::new(tool));
        }

        let storage = Arc::new(InMemoryStorage::new());
        let agent = Agent::new(
            self.provider.clone(),
            tools,
            Some(system_prompt),
            storage,
        );

        let conversation_id = format!("subagent-{}", uuid::Uuid::new_v4());

        log::info!("Spawning sub-agent '{}', task: {}", agent_name, task);

        // Read the current approval handler from the shared slot and wrap with context
        let handler_arc = self.approval_handler.read().await.clone();
        let contextual: Option<Arc<dyn crate::core::approval::ApprovalHandler>> =
            handler_arc.map(|h| {
                Arc::new(ContextualApprovalHandler::new(
                    h,
                    format!("sub-agent:{agent_name}"),
                )) as Arc<dyn crate::core::approval::ApprovalHandler>
            });
        let handler_ref = contextual.as_ref().map(|h| h.as_ref());

        match agent.process(&conversation_id, task, handler_ref).await {
            Ok(response) => {
                log::info!("Sub-agent '{}' completed successfully", agent_name);
                Ok(response)
            }
            Err(e) => {
                log::error!("Sub-agent '{}' failed: {}", agent_name, e);
                Err(format!("Sub-agent '{agent_name}' error: {e}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::approval::new_shared_approval_handler;
    use crate::core::{LlmError, LlmProvider, LlmResponse, Message, ToolDefinition};

    struct DummyProvider;

    #[async_trait]
    impl LlmProvider for DummyProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolDefinition],
        ) -> Result<LlmResponse, LlmError> {
            Ok(LlmResponse {
                content: Some("done".to_string()),
                tool_calls: vec![],
                usage: None,
            })
        }
    }

    #[test]
    fn validate_agent_name_valid() {
        assert!(validate_agent_name("my-agent").is_ok());
        assert!(validate_agent_name("agent_v2").is_ok());
        assert!(validate_agent_name("a").is_ok());
    }

    #[test]
    fn validate_agent_name_empty() {
        assert!(validate_agent_name("").is_err());
    }

    #[test]
    fn validate_agent_name_path_traversal() {
        assert!(validate_agent_name("..").is_err());
        assert!(validate_agent_name("../etc/passwd").is_err());
        assert!(validate_agent_name("foo/bar").is_err());
        assert!(validate_agent_name("foo\\bar").is_err());
    }

    #[tokio::test]
    async fn execute_rejects_at_max_depth() {
        let dir = tempfile::tempdir().unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(DummyProvider);
        let handler = new_shared_approval_handler();
        let tool = SpawnSubAgentTool::with_depth(
            provider,
            dir.path().to_path_buf(),
            handler,
            MAX_SPAWN_DEPTH,
        );

        let args = serde_json::json!({
            "agent_name": "test",
            "task": "do something"
        });
        let result = tool.execute(&args.to_string()).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("depth limit"));
    }

    #[test]
    fn list_available_agents_empty() {
        let dir = tempfile::tempdir().unwrap();
        let provider: Arc<dyn LlmProvider> = Arc::new(DummyProvider);
        let handler = new_shared_approval_handler();
        let tool = SpawnSubAgentTool::new(provider, dir.path().to_path_buf(), handler);
        assert!(tool.list_available_agents().is_empty());
    }

    #[test]
    fn list_available_agents_with_files() {
        let dir = tempfile::tempdir().unwrap();
        let subagents = dir.path().join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        std::fs::write(subagents.join("beta.md"), "prompt").unwrap();
        std::fs::write(subagents.join("alpha.md"), "prompt").unwrap();
        std::fs::write(subagents.join("notes.txt"), "ignored").unwrap();

        let provider: Arc<dyn LlmProvider> = Arc::new(DummyProvider);
        let handler = new_shared_approval_handler();
        let tool = SpawnSubAgentTool::new(provider, dir.path().to_path_buf(), handler);
        let agents = tool.list_available_agents();
        assert_eq!(agents, vec!["alpha", "beta"]);
    }
}
