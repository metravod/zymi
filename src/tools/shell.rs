use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::process::Command;

use crate::core::ToolDefinition;
use crate::policy::{PolicyDecision, PolicyEngine};
use crate::sandbox::{ExecutionContext, SandboxManager};
use crate::task_registry::{SharedTaskRegistry, TaskEntry, TaskKind};
use crate::tools::Tool;

const MAX_COMMAND_LENGTH: usize = 10_000;
const MAX_OUTPUT_LENGTH: usize = 30_000;
const DEFAULT_TIMEOUT_SECS: u64 = 300; // 5 minutes

pub struct ShellTool {
    task_registry: Option<SharedTaskRegistry>,
    policy: Option<Arc<PolicyEngine>>,
    sandbox: Option<Arc<SandboxManager>>,
    execution_context: ExecutionContext,
}

impl ShellTool {
    pub fn new() -> Self {
        Self {
            task_registry: None,
            policy: None,
            sandbox: None,
            execution_context: ExecutionContext::Interactive,
        }
    }

    pub fn with_task_registry(mut self, registry: SharedTaskRegistry) -> Self {
        self.task_registry = Some(registry);
        self
    }

    pub fn with_policy(mut self, policy: Arc<PolicyEngine>) -> Self {
        self.policy = Some(policy);
        self
    }

    pub fn with_sandbox(mut self, sandbox: Arc<SandboxManager>, context: ExecutionContext) -> Self {
        self.sandbox = Some(sandbox);
        self.execution_context = context;
        self
    }
}

/// Run a shell command with an optional timeout. Returns stdout+stderr.
///
/// If the command uses `sudo` and `SUDO_PASSWORD` is set, the password is
/// piped to sudo via stdin (`-S` flag) so non-interactive execution works.
pub(crate) async fn run_command(
    command: &str,
    timeout_secs: u64,
    sandbox: Option<&SandboxManager>,
    context: ExecutionContext,
) -> Result<String, String> {
    let is_docker = tokio::fs::try_exists("/.dockerenv")
        .await
        .unwrap_or(false);

    // Sudo handling: pipe password via stdin when available
    let sudo_password = if command.contains("sudo ") {
        std::env::var("SUDO_PASSWORD").ok().filter(|p| !p.is_empty())
    } else {
        None
    };

    let effective_command = if sudo_password.is_some() {
        command.replace("sudo ", "sudo -S ")
    } else {
        command.to_string()
    };

    let mut cmd = if let Some(sb) = sandbox.filter(|s| s.is_active()) {
        let sandboxed = sb.wrap_command(context, &effective_command, None);
        let mut c = Command::new(&sandboxed.program);
        c.args(&sandboxed.args);
        for (k, v) in &sandboxed.env {
            c.env(k, v);
        }
        c
    } else if is_docker {
        let mut c = Command::new("nsenter");
        c.args(["-t", "1", "-m", "-u", "-i", "-n", "--", "sh", "-c", &effective_command]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", &effective_command]);
        c
    };

    let output = if let Some(ref password) = sudo_password {
        cmd.stdin(std::process::Stdio::piped());
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("Failed to execute command: {e}"))?;

        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(format!("{password}\n").as_bytes()).await;
            drop(stdin);
        }

        tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output())
            .await
            .map_err(|_| {
                format!(
                    "Command timed out after {timeout_secs}s. The process was killed.\n\
                    Consider running with background=true for long commands."
                )
            })?
            .map_err(|e| format!("Failed to execute command: {e}"))?
    } else {
        tokio::time::timeout(Duration::from_secs(timeout_secs), cmd.output())
            .await
            .map_err(|_| {
                format!(
                    "Command timed out after {timeout_secs}s. The process was killed.\n\
                    Consider running with background=true for long commands."
                )
            })?
            .map_err(|e| format!("Failed to execute command: {e}"))?
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr_raw = String::from_utf8_lossy(&output.stderr);
    let exit_code = output.status.code().unwrap_or(-1);

    // Strip sudo password prompts from stderr (noise when using SUDO_PASSWORD)
    let stderr: String = if sudo_password.is_some() {
        stderr_raw
            .lines()
            .filter(|l| !l.starts_with("[sudo] password for"))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        stderr_raw.to_string()
    };

    let mut result = String::new();
    if !stdout.is_empty() {
        result.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str("[stderr] ");
        result.push_str(&stderr);
    }
    if result.is_empty() {
        result = format!("Command completed with exit code {exit_code}");
    } else if exit_code != 0 {
        result.push_str(&format!("\n[exit code: {exit_code}]"));
    }

    if result.len() > MAX_OUTPUT_LENGTH {
        let end = result.floor_char_boundary(MAX_OUTPUT_LENGTH);
        return Ok(format!(
            "{}\n\n[Output truncated at {} characters]",
            &result[..end],
            MAX_OUTPUT_LENGTH
        ));
    }

    Ok(result)
}

#[async_trait]
impl Tool for ShellTool {
    fn definition(&self) -> ToolDefinition {
        let bg_note = if self.task_registry.is_some() {
            " Set background=true to run asynchronously — returns a task_id you can check with check_task."
        } else {
            ""
        };

        ToolDefinition {
            name: "execute_shell".to_string(),
            description: format!(
                "Execute a shell command and return its output (stdout and stderr). \
                You can run any shell command: install packages (pip, brew, apt, npm), \
                create and run scripts, pipe commands, use CLI tools (ffmpeg, curl, jq, etc.). \
                If a tool is not installed, install it first. \
                Sudo is handled automatically — just use `sudo` normally, the password \
                is piped via stdin if SUDO_PASSWORD is set. Do NOT ask the user for sudo \
                passwords or construct manual sudo -S commands. \
                Commands have a default timeout of {DEFAULT_TIMEOUT_SECS}s — set timeout_secs \
                to override.{bg_note}"
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute"
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Timeout in seconds (default: 300). The process is killed if it exceeds this."
                    },
                    "background": {
                        "type": "boolean",
                        "description": "If true, run in background and return a task_id immediately. Use check_task to get the result later."
                    }
                },
                "required": ["command"]
            }),
        }
    }

    fn requires_approval(&self) -> bool {
        true
    }

    fn requires_approval_for(&self, arguments: &str) -> bool {
        // If sandbox is active and the profile says auto_approve, skip approval
        if let Some(ref sb) = self.sandbox {
            if sb.is_active() && sb.profile_for(self.execution_context).auto_approve {
                return false;
            }
        }

        if let Some(ref policy) = self.policy {
            if policy.is_enabled() {
                if let Ok(args) = serde_json::from_str::<serde_json::Value>(arguments) {
                    if let Some(command) = args["command"].as_str() {
                        return match policy.evaluate(command) {
                            PolicyDecision::Allow => false,
                            PolicyDecision::RequireApproval => true,
                            PolicyDecision::Deny(_) => true, // will be denied in execute()
                        };
                    }
                }
            }
        }
        true
    }

    fn format_approval_request(&self, arguments: &str) -> String {
        let command = serde_json::from_str::<serde_json::Value>(arguments)
            .ok()
            .and_then(|v| v["command"].as_str().map(String::from))
            .unwrap_or_else(|| arguments.to_string());
        format!("Command:\n<code>{}</code>", command)
    }

    async fn execute(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid arguments: {e}"))?;

        let command = args["command"]
            .as_str()
            .ok_or("Missing required parameter: command")?;

        if command.len() > MAX_COMMAND_LENGTH {
            return Err(format!(
                "Command too long ({} chars, max {MAX_COMMAND_LENGTH})",
                command.len()
            ));
        }

        // Policy engine check
        if let Some(ref policy) = self.policy {
            if let PolicyDecision::Deny(reason) = policy.evaluate(command) {
                return Err(format!("Policy denied: {reason}"));
            }
            // Allow and RequireApproval both proceed — approval is handled by
            // the agent's approval handler (requires_approval() returns true).
        }

        let timeout_secs = args["timeout_secs"]
            .as_u64()
            .unwrap_or(DEFAULT_TIMEOUT_SECS);

        let background = args["background"].as_bool().unwrap_or(false);

        // Background mode: spawn and return task_id
        if background {
            let registry = self.task_registry.as_ref().ok_or(
                "Background execution is not available in this context. Run without background=true.",
            )?;

            let task_id = uuid::Uuid::new_v4().to_string();
            let cmd_display = if command.len() > 60 {
                format!("{}...", &command[..60])
            } else {
                command.to_string()
            };

            let entry = TaskEntry::new(
                task_id.clone(),
                TaskKind::Shell {
                    command: command.to_string(),
                },
                cmd_display,
            );

            registry.write().await.insert(entry);

            let reg = registry.clone();
            let tid = task_id.clone();
            let cmd = command.to_string();
            let bg_sandbox = self.sandbox.clone();
            let bg_context = self.execution_context;

            tokio::spawn(async move {
                reg.write().await.set_running(&tid);
                log::info!("Task {tid}: running background shell: {}", &cmd[..cmd.len().min(100)]);

                match run_command(&cmd, timeout_secs, bg_sandbox.as_deref(), bg_context).await {
                    Ok(result) => {
                        log::info!("Task {tid}: shell completed, result_len={}", result.len());
                        reg.write().await.set_completed(&tid, result);
                    }
                    Err(error) => {
                        log::error!("Task {tid}: shell failed: {error}");
                        reg.write().await.set_failed(&tid, error);
                    }
                }
            });

            return Ok(format!(
                "Command launched in background.\n\
                task_id: {task_id}\n\
                timeout: {timeout_secs}s\n\n\
                Use check_task with this task_id to get the result."
            ));
        }

        // Foreground mode: run with timeout
        run_command(command, timeout_secs, self.sandbox.as_deref(), self.execution_context).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(command: &str) -> String {
        serde_json::json!({ "command": command }).to_string()
    }

    #[tokio::test]
    async fn execute_echo() {
        let tool = ShellTool::new();
        let result = tool.execute(&args("echo hello")).await.unwrap();
        assert!(result.contains("hello"));
    }

    #[tokio::test]
    async fn execute_exit_code() {
        let tool = ShellTool::new();
        let result = tool.execute(&args("false")).await.unwrap();
        assert!(result.contains("exit code"));
    }

    #[tokio::test]
    async fn execute_stderr() {
        let tool = ShellTool::new();
        let result = tool.execute(&args("echo err >&2")).await.unwrap();
        assert!(result.contains("[stderr]"));
        assert!(result.contains("err"));
    }

    #[tokio::test]
    async fn execute_invalid_json() {
        let tool = ShellTool::new();
        let result = tool.execute("not json").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid arguments"));
    }

    #[tokio::test]
    async fn execute_missing_command() {
        let tool = ShellTool::new();
        let result = tool.execute("{}").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Missing"));
    }

    #[tokio::test]
    async fn execute_command_too_long() {
        let tool = ShellTool::new();
        let long_cmd = "x".repeat(MAX_COMMAND_LENGTH + 1);
        let result = tool.execute(&args(&long_cmd)).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("too long"));
    }

    #[test]
    fn requires_approval_true() {
        let tool = ShellTool::new();
        assert!(tool.requires_approval());
    }

    #[test]
    fn format_approval_request_parses_json() {
        let tool = ShellTool::new();
        let desc = tool.format_approval_request(&args("ls -la"));
        assert!(desc.contains("ls -la"));
    }

    #[test]
    fn format_approval_request_fallback() {
        let tool = ShellTool::new();
        let desc = tool.format_approval_request("not json");
        assert!(desc.contains("not json"));
    }

    #[tokio::test]
    async fn execute_timeout() {
        let tool = ShellTool::new();
        let args = serde_json::json!({
            "command": "sleep 10",
            "timeout_secs": 1
        })
        .to_string();
        let result = tool.execute(&args).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("timed out"));
    }

    #[tokio::test]
    async fn execute_background_without_registry() {
        let tool = ShellTool::new();
        let args = serde_json::json!({
            "command": "echo hi",
            "background": true
        })
        .to_string();
        let result = tool.execute(&args).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not available"));
    }

    #[tokio::test]
    async fn execute_background_with_registry() {
        let registry = crate::task_registry::new_task_registry();
        let tool = ShellTool::new().with_task_registry(registry.clone());
        let args = serde_json::json!({
            "command": "echo background-test",
            "background": true
        })
        .to_string();
        let result = tool.execute(&args).await.unwrap();
        assert!(result.contains("task_id:"));
        assert!(result.contains("background"));

        // Wait for the background task to complete (poll instead of fixed sleep)
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let reg = registry.read().await;
            let tasks = reg.list();
            if !tasks.is_empty()
                && tasks[0].status == crate::task_registry::TaskStatus::Completed
            {
                break;
            }
        }

        let reg = registry.read().await;
        let tasks = reg.list();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status, crate::task_registry::TaskStatus::Completed);
        assert!(tasks[0].result.as_ref().unwrap().contains("background-test"));
    }

    #[tokio::test]
    async fn execute_output_truncation() {
        let tool = ShellTool::new();
        let count = MAX_OUTPUT_LENGTH + 5000;
        let cmd = format!("printf 'x%.0s' $(seq 1 {count})");
        let result = tool.execute(&args(&cmd)).await.unwrap();
        assert!(result.contains("[Output truncated"));
        assert!(result.len() <= MAX_OUTPUT_LENGTH + 100);
    }

    // --- Sandbox integration tests ---

    #[test]
    fn no_sandbox_requires_approval() {
        let tool = ShellTool::new();
        assert!(tool.requires_approval_for(&args("echo hi")));
    }

    #[tokio::test]
    async fn sandbox_disabled_requires_approval() {
        use crate::sandbox::{SandboxConfig, SandboxManager};
        let config = SandboxConfig::default(); // disabled
        let mgr = Arc::new(SandboxManager::new(config).await);
        let tool = ShellTool::new().with_sandbox(mgr, ExecutionContext::Workflow);
        // Sandbox disabled → still requires approval
        assert!(tool.requires_approval_for(&args("echo hi")));
    }

    #[tokio::test]
    async fn sandbox_native_auto_approve_still_requires() {
        use std::collections::HashMap;
        use crate::sandbox::{
            SandboxBackendType, SandboxConfig, SandboxManager, SandboxProfile,
        };
        // Enabled but native backend → is_active() returns false
        let config = SandboxConfig {
            enabled: true,
            default_backend: SandboxBackendType::Native,
            profiles: HashMap::from([(
                ExecutionContext::Workflow,
                SandboxProfile {
                    auto_approve: true,
                    ..Default::default()
                },
            )]),
        };
        let mgr = Arc::new(SandboxManager::new(config).await);
        let tool = ShellTool::new().with_sandbox(mgr, ExecutionContext::Workflow);
        // Native backend → is_active() false → auto_approve ignored
        assert!(tool.requires_approval_for(&args("echo hi")));
    }

    #[test]
    fn with_sandbox_sets_context() {
        use crate::sandbox::{SandboxConfig, SandboxManager};
        // Just verify the builder sets the context correctly
        let tool = ShellTool::new();
        assert_eq!(tool.execution_context, ExecutionContext::Interactive);

        // Can't test with_sandbox without async SandboxManager::new,
        // but verify the field is there via the default
    }
}
