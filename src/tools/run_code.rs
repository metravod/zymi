use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::process::Command;

use crate::core::ToolDefinition;
use crate::sandbox::SandboxManager;

use super::Tool;

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_CODE_LENGTH: usize = 50_000;

pub struct RunCodeTool {
    sandbox: Option<Arc<SandboxManager>>,
}

impl RunCodeTool {
    pub fn new() -> Self {
        Self { sandbox: None }
    }

    pub fn with_sandbox(mut self, sandbox: Arc<SandboxManager>) -> Self {
        self.sandbox = Some(sandbox);
        self
    }
}

#[async_trait]
impl Tool for RunCodeTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "run_code".to_string(),
            description: "Write and execute code (Python, Bash, or Node.js). \
                Use for data processing, file manipulation, API calls, calculations, \
                image/video/audio processing, scraping, or any task easier to solve with code. \
                The code is written to a temp file and executed. \
                Prefer Python for complex logic, Bash for simple pipelines."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "language": {
                        "type": "string",
                        "enum": ["python", "bash", "node"],
                        "description": "The programming language to use"
                    },
                    "code": {
                        "type": "string",
                        "description": "The code to execute"
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Timeout in seconds (default: 120)"
                    }
                },
                "required": ["language", "code"]
            }),
        }
    }

    fn requires_approval(&self) -> bool {
        true
    }

    fn format_approval_request(&self, arguments: &str) -> String {
        let (lang, code) = serde_json::from_str::<serde_json::Value>(arguments)
            .ok()
            .map(|v| {
                let lang = v["language"].as_str().unwrap_or("?").to_string();
                let code = v["code"].as_str().unwrap_or("").to_string();
                (lang, code)
            })
            .unwrap_or_else(|| ("?".to_string(), arguments.to_string()));

        format!("Run {lang}:\n{code}")
    }

    async fn execute(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid arguments: {e}"))?;

        let language = args["language"]
            .as_str()
            .ok_or("Missing required parameter: language")?;

        let code = args["code"]
            .as_str()
            .ok_or("Missing required parameter: code")?;

        if code.len() > MAX_CODE_LENGTH {
            return Err(format!(
                "Code too long ({} chars, max {MAX_CODE_LENGTH})",
                code.len()
            ));
        }

        let timeout_secs = args["timeout_secs"]
            .as_u64()
            .unwrap_or(DEFAULT_TIMEOUT_SECS);

        let (extension, interpreter) = match language {
            "python" => ("py", "python3"),
            "bash" => ("sh", "sh"),
            "node" => ("js", "node"),
            _ => return Err(format!("Unsupported language: {language}. Use python, bash, or node.")),
        };

        // Write to temp file
        let tmp_dir = std::env::temp_dir();
        let file_name = format!("zymi_run_{}.{extension}", uuid::Uuid::new_v4().simple());
        let file_path = tmp_dir.join(&file_name);

        tokio::fs::write(&file_path, code)
            .await
            .map_err(|e| format!("Failed to write temp file: {e}"))?;

        // Execute
        let result = run_script(interpreter, &file_path, timeout_secs, self.sandbox.as_deref()).await;

        // Cleanup
        let _ = tokio::fs::remove_file(&file_path).await;

        result
    }
}

async fn run_script(
    interpreter: &str,
    file_path: &std::path::Path,
    timeout_secs: u64,
    sandbox: Option<&SandboxManager>,
) -> Result<String, String> {
    use crate::sandbox::ExecutionContext;

    let child_future = if let Some(sb) = sandbox.filter(|s| s.is_active()) {
        let sandboxed =
            sb.wrap_script(ExecutionContext::RunCode, interpreter, file_path, None);
        let mut cmd = Command::new(&sandboxed.program);
        cmd.args(&sandboxed.args);
        for (k, v) in &sandboxed.env {
            cmd.env(k, v);
        }
        cmd.output()
    } else {
        Command::new(interpreter).arg(file_path).output()
    };

    let output = tokio::time::timeout(Duration::from_secs(timeout_secs), child_future)
        .await
        .map_err(|_| format!("Code execution timed out after {timeout_secs}s"))?
        .map_err(|e| format!("Failed to execute {interpreter}: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let exit_code = output.status.code().unwrap_or(-1);

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
        result = format!("Code completed with exit code {exit_code}");
    } else if exit_code != 0 {
        result.push_str(&format!("\n[exit code: {exit_code}]"));
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn execute_python_hello() {
        let tool = RunCodeTool::new();
        let result = tool
            .execute(r#"{"language": "python", "code": "print('hello')"}"#)
            .await
            .unwrap();
        assert_eq!(result.trim(), "hello");
    }

    #[tokio::test]
    async fn execute_bash_echo() {
        let tool = RunCodeTool::new();
        let result = tool
            .execute(r#"{"language": "bash", "code": "echo 42"}"#)
            .await
            .unwrap();
        assert_eq!(result.trim(), "42");
    }

    #[tokio::test]
    async fn execute_python_multiline() {
        let tool = RunCodeTool::new();
        let code = "import json\ndata = {'a': 1}\nprint(json.dumps(data))";
        let args = serde_json::json!({"language": "python", "code": code}).to_string();
        let result = tool.execute(&args).await.unwrap();
        assert_eq!(result.trim(), r#"{"a": 1}"#);
    }

    #[tokio::test]
    async fn execute_python_error() {
        let tool = RunCodeTool::new();
        let result = tool
            .execute(r#"{"language": "python", "code": "raise ValueError('oops')"}"#)
            .await
            .unwrap();
        assert!(result.contains("ValueError"));
        assert!(result.contains("exit code: 1"));
    }

    #[tokio::test]
    async fn execute_unsupported_language() {
        let tool = RunCodeTool::new();
        let result = tool
            .execute(r#"{"language": "ruby", "code": "puts 1"}"#)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unsupported language"));
    }

    #[tokio::test]
    async fn execute_missing_code() {
        let tool = RunCodeTool::new();
        let result = tool.execute(r#"{"language": "python"}"#).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn execute_code_too_long() {
        let tool = RunCodeTool::new();
        let long_code = "x = 1\n".repeat(MAX_CODE_LENGTH);
        let args = serde_json::json!({"language": "python", "code": long_code}).to_string();
        let result = tool.execute(&args).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("too long"));
    }

    #[tokio::test]
    async fn format_approval_shows_code() {
        let tool = RunCodeTool::new();
        let desc = tool.format_approval_request(r#"{"language": "python", "code": "print(1)"}"#);
        assert!(desc.contains("python"));
        assert!(desc.contains("print(1)"));
    }

    #[test]
    fn requires_approval_true() {
        let tool = RunCodeTool::new();
        assert!(tool.requires_approval());
    }
}
