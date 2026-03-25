use std::path::PathBuf;

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::core::ToolDefinition;
use crate::scheduler::{load_schedule, parse_cron, save_schedule, ScheduleEntry};
use crate::tools::Tool;

pub struct ManageScheduleTool {
    memory_dir: PathBuf,
}

impl ManageScheduleTool {
    pub fn new(memory_dir: PathBuf) -> Self {
        Self { memory_dir }
    }

    fn generate_id() -> String {
        uuid::Uuid::new_v4().to_string()[..8].to_string()
    }

    fn handle_create(&self, args: &serde_json::Value) -> Result<String, String> {
        let name = args
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or("Missing required parameter: name")?;

        let task = args
            .get("task")
            .and_then(|v| v.as_str())
            .ok_or("Missing required parameter: task")?;

        let cron_expr = args.get("cron").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty() && *s != ".");
        let once_at_str = args.get("once_at").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty());

        if cron_expr.is_none() && once_at_str.is_none() {
            return Err("Either 'cron' or 'once_at' must be provided".to_string());
        }
        if cron_expr.is_some() && once_at_str.is_some() {
            return Err("Only one of 'cron' or 'once_at' should be provided".to_string());
        }

        let cron = if let Some(expr) = cron_expr {
            parse_cron(expr)?;
            Some(expr.to_string())
        } else {
            None
        };

        let once_at = if let Some(dt_str) = once_at_str {
            let dt = DateTime::parse_from_rfc3339(dt_str)
                .map_err(|e| format!("Invalid once_at datetime (expected RFC3339): {e}"))?;
            Some(dt.with_timezone(&Utc))
        } else {
            None
        };

        let id = Self::generate_id();
        let entry = ScheduleEntry {
            id: id.clone(),
            name: name.to_string(),
            task: task.to_string(),
            cron,
            once_at,
            last_run: None,
            created_at: Utc::now(),
        };

        let mut entries = load_schedule(&self.memory_dir);
        entries.push(entry);
        save_schedule(&self.memory_dir, &entries);

        let schedule_type = if let Some(cron) = cron_expr {
            format!("cron: {cron}")
        } else {
            format!("once_at: {}", once_at_str.unwrap())
        };

        Ok(format!(
            "Created scheduled task:\n  id: {id}\n  name: {name}\n  schedule: {schedule_type}\n  task: {task}"
        ))
    }

    fn handle_list(&self) -> Result<String, String> {
        let entries = load_schedule(&self.memory_dir);
        if entries.is_empty() {
            return Ok("No scheduled tasks.".to_string());
        }

        let mut output = format!("Scheduled tasks ({}):\n", entries.len());
        for e in &entries {
            let schedule = if let Some(ref c) = e.cron {
                format!("cron: {c}")
            } else if let Some(ref o) = e.once_at {
                format!("once_at: {o}")
            } else {
                "none".to_string()
            };

            let last_run = match e.last_run {
                Some(ref lr) => lr.to_string(),
                None => "never".to_string(),
            };

            output.push_str(&format!(
                "\n- [{}] {}\n  schedule: {}\n  task: {}\n  last_run: {}\n",
                e.id, e.name, schedule, e.task, last_run
            ));
        }

        Ok(output)
    }

    fn handle_delete(&self, args: &serde_json::Value) -> Result<String, String> {
        let id = args
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or("Missing required parameter: id")?;

        let mut entries = load_schedule(&self.memory_dir);
        let before_len = entries.len();
        entries.retain(|e| e.id != id);

        if entries.len() == before_len {
            return Err(format!("Schedule entry with id '{id}' not found"));
        }

        save_schedule(&self.memory_dir, &entries);
        Ok(format!("Deleted schedule entry '{id}'"))
    }
}

#[async_trait]
impl Tool for ManageScheduleTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "manage_schedule".to_string(),
            description: "Manage scheduled tasks that run automatically. Actions: 'create' (set up a recurring cron or one-time task), 'list' (show all scheduled tasks), 'delete' (remove a task by id). Scheduled tasks run in an isolated agent context with access to time and memory tools only.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["create", "list", "delete"],
                        "description": "Action to perform"
                    },
                    "name": {
                        "type": "string",
                        "description": "Name/description of the scheduled task (for 'create')"
                    },
                    "task": {
                        "type": "string",
                        "description": "The task prompt that the scheduled agent will execute (for 'create')"
                    },
                    "cron": {
                        "type": "string",
                        "description": "Standard 5-field cron expression, e.g. '*/5 * * * *' for every 5 minutes (for 'create', mutually exclusive with once_at)"
                    },
                    "once_at": {
                        "type": "string",
                        "description": "RFC3339 datetime for one-time execution, e.g. '2026-03-01T10:00:00Z' (for 'create', mutually exclusive with cron)"
                    },
                    "id": {
                        "type": "string",
                        "description": "ID of the schedule entry to delete (for 'delete')"
                    }
                },
                "required": ["action"]
            }),
        }
    }

    async fn execute(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid arguments: {e}"))?;

        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .ok_or("Missing required parameter: action")?;

        match action {
            "create" => self.handle_create(&args),
            "list" => self.handle_list(),
            "delete" => self.handle_delete(&args),
            _ => Err(format!("Unknown action: {action}. Use 'create', 'list', or 'delete'.")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tool(dir: &std::path::Path) -> ManageScheduleTool {
        ManageScheduleTool::new(dir.to_path_buf())
    }

    #[tokio::test]
    async fn handle_create_cron() {
        let dir = tempfile::tempdir().unwrap();
        let tool = make_tool(dir.path());
        let args = serde_json::json!({
            "action": "create",
            "name": "daily check",
            "task": "check status",
            "cron": "0 9 * * *"
        });
        let result = tool.execute(&args.to_string()).await.unwrap();
        assert!(result.contains("Created scheduled task"));
        assert!(result.contains("daily check"));
        assert!(result.contains("cron: 0 9 * * *"));
    }

    #[tokio::test]
    async fn handle_create_once_at() {
        let dir = tempfile::tempdir().unwrap();
        let tool = make_tool(dir.path());
        let args = serde_json::json!({
            "action": "create",
            "name": "one-time",
            "task": "run migration",
            "once_at": "2026-12-01T10:00:00Z"
        });
        let result = tool.execute(&args.to_string()).await.unwrap();
        assert!(result.contains("Created scheduled task"));
        assert!(result.contains("once_at:"));
    }

    #[tokio::test]
    async fn handle_create_missing_schedule() {
        let dir = tempfile::tempdir().unwrap();
        let tool = make_tool(dir.path());
        let args = serde_json::json!({
            "action": "create",
            "name": "bad",
            "task": "something"
        });
        let result = tool.execute(&args.to_string()).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("cron"));
    }

    #[tokio::test]
    async fn handle_create_both_provided() {
        let dir = tempfile::tempdir().unwrap();
        let tool = make_tool(dir.path());
        let args = serde_json::json!({
            "action": "create",
            "name": "both",
            "task": "do",
            "cron": "* * * * *",
            "once_at": "2026-12-01T10:00:00Z"
        });
        let result = tool.execute(&args.to_string()).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Only one"));
    }

    #[tokio::test]
    async fn handle_create_invalid_cron() {
        let dir = tempfile::tempdir().unwrap();
        let tool = make_tool(dir.path());
        let args = serde_json::json!({
            "action": "create",
            "name": "bad-cron",
            "task": "do",
            "cron": "not a cron"
        });
        let result = tool.execute(&args.to_string()).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid cron"));
    }

    #[tokio::test]
    async fn handle_list_empty() {
        let dir = tempfile::tempdir().unwrap();
        let tool = make_tool(dir.path());
        let result = tool.execute(r#"{"action":"list"}"#).await.unwrap();
        assert_eq!(result, "No scheduled tasks.");
    }

    #[tokio::test]
    async fn handle_list_with_entries() {
        let dir = tempfile::tempdir().unwrap();
        let tool = make_tool(dir.path());

        // Create an entry first
        let args = serde_json::json!({
            "action": "create",
            "name": "test-task",
            "task": "hello",
            "cron": "*/5 * * * *"
        });
        tool.execute(&args.to_string()).await.unwrap();

        let result = tool.execute(r#"{"action":"list"}"#).await.unwrap();
        assert!(result.contains("Scheduled tasks (1):"));
        assert!(result.contains("test-task"));
        assert!(result.contains("*/5 * * * *"));
    }

    #[tokio::test]
    async fn handle_delete_existing() {
        let dir = tempfile::tempdir().unwrap();
        let tool = make_tool(dir.path());

        // Create an entry
        let args = serde_json::json!({
            "action": "create",
            "name": "to-delete",
            "task": "hello",
            "cron": "* * * * *"
        });
        let create_result = tool.execute(&args.to_string()).await.unwrap();

        // Extract id from output
        let id = create_result
            .lines()
            .find(|l| l.contains("id:"))
            .unwrap()
            .split("id:")
            .nth(1)
            .unwrap()
            .trim();

        let del = serde_json::json!({"action":"delete","id":id});
        let result = tool.execute(&del.to_string()).await.unwrap();
        assert!(result.contains("Deleted"));

        // Verify empty
        let list = tool.execute(r#"{"action":"list"}"#).await.unwrap();
        assert_eq!(list, "No scheduled tasks.");
    }

    #[tokio::test]
    async fn handle_delete_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let tool = make_tool(dir.path());
        let args = serde_json::json!({"action":"delete","id":"nonexistent"});
        let result = tool.execute(&args.to_string()).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }
}
