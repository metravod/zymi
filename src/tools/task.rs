use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use crate::core::agent::Agent;
use crate::core::approval::{ContextualApprovalHandler, SharedApprovalHandler};
use crate::core::{LlmProvider, ToolDefinition};
use crate::sandbox::{ExecutionContext, SandboxManager};
use crate::storage::in_memory::InMemoryStorage;
use crate::task_registry::{new_task_registry, SharedTaskRegistry, TaskEntry, TaskKind, TaskStatus};
use crate::tools::current_time::CurrentTimeTool;
use crate::tools::memory::{ReadMemoryTool, WriteMemoryTool};
use crate::tools::shell::ShellTool;
use crate::tools::web_scrape::WebScrapeTool;
use crate::tools::web_search::WebSearchTool;
use crate::tools::Tool;

const TASK_MAX_ITERATIONS: usize = 25;

// ---------------------------------------------------------------------------
// spawn_task — launch an async sub-agent, return immediately
// ---------------------------------------------------------------------------

pub struct SpawnTaskTool {
    provider: Arc<dyn LlmProvider>,
    memory_dir: PathBuf,
    subagents_dir: PathBuf,
    approval_handler: SharedApprovalHandler,
    registry: SharedTaskRegistry,
    sandbox: Option<Arc<SandboxManager>>,
}

impl SpawnTaskTool {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        memory_dir: PathBuf,
        approval_handler: SharedApprovalHandler,
        registry: SharedTaskRegistry,
    ) -> Self {
        let subagents_dir = memory_dir.join("subagents");
        Self {
            provider,
            memory_dir,
            subagents_dir,
            approval_handler,
            registry,
            sandbox: None,
        }
    }

    pub fn with_sandbox(mut self, sandbox: Arc<SandboxManager>) -> Self {
        self.sandbox = Some(sandbox);
        self
    }

    fn list_available_agents(&self) -> Vec<String> {
        let entries = match std::fs::read_dir(&self.subagents_dir) {
            Ok(e) => e,
            Err(_) => return vec![],
        };
        let mut agents: Vec<String> = entries
            .filter_map(|e| {
                let name = e.ok()?.file_name().to_string_lossy().to_string();
                name.strip_suffix(".md").map(String::from)
            })
            .collect();
        agents.sort();
        agents
    }
}

fn validate_agent_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Agent name is required.".into());
    }
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err("Invalid agent name: path traversal is not allowed.".into());
    }
    Ok(())
}

#[async_trait]
impl Tool for SpawnTaskTool {
    fn definition(&self) -> ToolDefinition {
        let agents = self.list_available_agents();
        let agents_list = if agents.is_empty() {
            "No sub-agents available yet.".into()
        } else {
            format!("Available agents: {}", agents.join(", "))
        };

        ToolDefinition {
            name: "spawn_task".to_string(),
            description: format!(
                "Launch a sub-agent as an ASYNC background task. Returns a task_id immediately — \
                use check_task to poll for results. The sub-agent runs with up to {TASK_MAX_ITERATIONS} \
                iterations and has access to: shell, memory, web_search, web_scrape. \
                Use this for long-running or independent work that doesn't need to block. {agents_list}"
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
                        "description": "Detailed task description for the sub-agent"
                    },
                    "description": {
                        "type": "string",
                        "description": "Short human-readable label for tracking (e.g. 'Install nginx on server')"
                    }
                },
                "required": ["agent_name", "task"]
            }),
        }
    }

    async fn execute(&self, arguments: &str) -> Result<String, String> {
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
            .ok_or("Missing required parameter: task")?
            .to_string();

        let description = args
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or(agent_name)
            .to_string();

        validate_agent_name(agent_name)?;

        let prompt_path = self.subagents_dir.join(format!("{agent_name}.md"));
        let system_prompt = tokio::fs::read_to_string(&prompt_path)
            .await
            .map_err(|e| format!("Cannot read sub-agent '{agent_name}': {e}"))?;

        let task_id = uuid::Uuid::new_v4().to_string();
        let entry = TaskEntry::new(
            task_id.clone(),
            TaskKind::Agent {
                agent_name: agent_name.to_string(),
            },
            description.clone(),
        );

        // Register task
        self.registry.write().await.insert(entry);

        // Clone what the spawned future needs
        let registry = self.registry.clone();
        let provider = self.provider.clone();
        let memory_dir = self.memory_dir.clone();
        let approval_handler = self.approval_handler.clone();
        let agent_name_owned = agent_name.to_string();
        let tid = task_id.clone();
        let sandbox = self.sandbox.clone();

        tokio::spawn(async move {
            // Mark running
            registry.write().await.set_running(&tid);

            log::info!("Task {tid}: starting sub-agent '{agent_name_owned}'");

            // Build sub-agent tools — same as SpawnSubAgentTool but with task registry
            let sub_registry = new_task_registry();
            let shell = {
                let mut s = ShellTool::new().with_task_registry(sub_registry);
                if let Some(ref sb) = sandbox {
                    s = s.with_sandbox(sb.clone(), ExecutionContext::SubAgent);
                }
                s
            };
            let mut tools: Vec<Box<dyn Tool>> = vec![
                Box::new(CurrentTimeTool),
                Box::new(ReadMemoryTool::new(memory_dir.clone())),
                Box::new(WriteMemoryTool::new(memory_dir.clone())),
                Box::new(shell),
            ];

            if let Some(tool) = WebSearchTool::new() {
                tools.push(Box::new(tool));
            }
            if let Some(tool) = WebScrapeTool::new() {
                tools.push(Box::new(tool));
            }

            let storage = Arc::new(InMemoryStorage::new());
            let agent = Agent::new(provider, tools, Some(system_prompt), storage)
                .with_max_iterations(TASK_MAX_ITERATIONS);

            let conversation_id = format!("task-{tid}");

            // Read approval handler
            let handler_arc = approval_handler.read().await.clone();
            let contextual: Option<Arc<dyn crate::core::approval::ApprovalHandler>> =
                handler_arc.map(|h| {
                    Arc::new(ContextualApprovalHandler::new(
                        h,
                        format!("task:{agent_name_owned}"),
                    )) as Arc<dyn crate::core::approval::ApprovalHandler>
                });
            let handler_ref = contextual.as_ref().map(|h| h.as_ref());

            match agent.process(&conversation_id, &task, handler_ref).await {
                Ok(result) => {
                    log::info!("Task {tid}: completed successfully");
                    registry.write().await.set_completed(&tid, result);
                }
                Err(e) => {
                    log::error!("Task {tid}: failed: {e}");
                    registry.write().await.set_failed(&tid, e.to_string());
                }
            }
        });

        Ok(format!(
            "Task spawned successfully.\n\
            task_id: {task_id}\n\
            description: {description}\n\
            status: running\n\n\
            Use check_task with this task_id to monitor progress."
        ))
    }
}

// ---------------------------------------------------------------------------
// check_task — poll status of a background task
// ---------------------------------------------------------------------------

pub struct CheckTaskTool {
    registry: SharedTaskRegistry,
}

impl CheckTaskTool {
    pub fn new(registry: SharedTaskRegistry) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for CheckTaskTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "check_task".to_string(),
            description: "Check the status and result of a background task launched by spawn_task \
                or a background shell command. Returns status (pending/running/completed/failed) \
                and the result if finished."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "The task ID returned by spawn_task or execute_shell (background mode)"
                    }
                },
                "required": ["task_id"]
            }),
        }
    }

    async fn execute(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid arguments: {e}"))?;

        let task_id = args
            .get("task_id")
            .and_then(|v| v.as_str())
            .ok_or("Missing required parameter: task_id")?;

        let registry = self.registry.read().await;
        let entry = registry
            .get(task_id)
            .ok_or_else(|| format!("No task found with id: {task_id}"))?;

        let mut output = format!(
            "task_id: {}\nstatus: {}\ndescription: {}\nelapsed: {:.1}s",
            entry.id,
            entry.status,
            entry.description,
            entry.elapsed_secs(),
        );

        match entry.status {
            TaskStatus::Completed => {
                if let Some(ref result) = entry.result {
                    output.push_str(&format!("\n\nresult:\n{result}"));
                }
            }
            TaskStatus::Failed => {
                if let Some(ref error) = entry.error {
                    output.push_str(&format!("\n\nerror: {error}"));
                }
            }
            TaskStatus::Pending | TaskStatus::Running => {
                output.push_str("\n\nTask is still in progress. Call check_task again later.");
            }
        }

        Ok(output)
    }
}

// ---------------------------------------------------------------------------
// list_tasks — overview of all tasks
// ---------------------------------------------------------------------------

pub struct ListTasksTool {
    registry: SharedTaskRegistry,
}

impl ListTasksTool {
    pub fn new(registry: SharedTaskRegistry) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for ListTasksTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "list_tasks".to_string(),
            description:
                "List all background tasks with their current status. Use this to see what's \
                running, completed, or failed before deciding next steps."
                    .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        }
    }

    async fn execute(&self, _arguments: &str) -> Result<String, String> {
        let registry = self.registry.read().await;
        let tasks = registry.list();

        if tasks.is_empty() {
            return Ok("No tasks have been created yet.".into());
        }

        let active = registry.active_count();

        let mut output = format!(
            "{} task(s) total, {} active\n\n",
            tasks.len(),
            active
        );

        for entry in &tasks {
            let kind = match &entry.kind {
                TaskKind::Agent { agent_name } => format!("agent:{agent_name}"),
                TaskKind::Shell { command } => {
                    let cmd = if command.len() > 40 {
                        format!("{}...", &command[..40])
                    } else {
                        command.clone()
                    };
                    format!("shell:{cmd}")
                }
            };

            let short_id = if entry.id.len() > 8 {
                &entry.id[..8]
            } else {
                &entry.id
            };
            output.push_str(&format!(
                "- [{}] {} | {} | {:.1}s | {}\n",
                entry.status,
                short_id,
                kind,
                entry.elapsed_secs(),
                entry.description,
            ));
        }

        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn check_task_not_found() {
        let registry = new_task_registry();
        let tool = CheckTaskTool::new(registry);
        let args = serde_json::json!({ "task_id": "nonexistent" }).to_string();
        let result = tool.execute(&args).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No task found"));
    }

    #[tokio::test]
    async fn list_tasks_empty() {
        let registry = new_task_registry();
        let tool = ListTasksTool::new(registry);
        let result = tool.execute("{}").await.unwrap();
        assert!(result.contains("No tasks"));
    }

    #[tokio::test]
    async fn list_tasks_with_entries() {
        let registry = new_task_registry();

        {
            let mut reg = registry.write().await;
            reg.insert(TaskEntry::new(
                "task-1".into(),
                TaskKind::Agent {
                    agent_name: "deployer".into(),
                },
                "Deploy to server".into(),
            ));
            reg.set_running("task-1");

            let mut entry2 = TaskEntry::new(
                "task-2".into(),
                TaskKind::Shell {
                    command: "apt install nginx".into(),
                },
                "Install nginx".into(),
            );
            entry2.status = TaskStatus::Completed;
            entry2.result = Some("done".into());
            reg.insert(entry2);
        }

        let tool = ListTasksTool::new(registry);
        let result = tool.execute("{}").await.unwrap();
        assert!(result.contains("2 task(s) total"));
        assert!(result.contains("1 active"));
        assert!(result.contains("deployer"));
        assert!(result.contains("nginx"));
    }

    #[tokio::test]
    async fn check_task_completed() {
        let registry = new_task_registry();

        {
            let mut reg = registry.write().await;
            reg.insert(TaskEntry::new(
                "task-abc".into(),
                TaskKind::Agent {
                    agent_name: "test".into(),
                },
                "Test task".into(),
            ));
            reg.set_completed("task-abc", "All done!".into());
        }

        let tool = CheckTaskTool::new(registry);
        let args = serde_json::json!({ "task_id": "task-abc" }).to_string();
        let result = tool.execute(&args).await.unwrap();
        assert!(result.contains("completed"));
        assert!(result.contains("All done!"));
    }

    #[tokio::test]
    async fn check_task_failed() {
        let registry = new_task_registry();

        {
            let mut reg = registry.write().await;
            reg.insert(TaskEntry::new(
                "task-fail".into(),
                TaskKind::Shell {
                    command: "bad cmd".into(),
                },
                "Bad command".into(),
            ));
            reg.set_failed("task-fail", "command not found".into());
        }

        let tool = CheckTaskTool::new(registry);
        let args = serde_json::json!({ "task_id": "task-fail" }).to_string();
        let result = tool.execute(&args).await.unwrap();
        assert!(result.contains("failed"));
        assert!(result.contains("command not found"));
    }
}
