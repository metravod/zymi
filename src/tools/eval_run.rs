use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use crate::core::{LlmProvider, ToolDefinition};
use crate::eval;
use crate::tools::Tool;

pub struct RunEvalsTool {
    provider: Arc<dyn LlmProvider>,
    memory_dir: PathBuf,
}

impl RunEvalsTool {
    pub fn new(provider: Arc<dyn LlmProvider>, memory_dir: PathBuf) -> Self {
        Self {
            provider,
            memory_dir,
        }
    }
}

#[async_trait]
impl Tool for RunEvalsTool {
    fn definition(&self) -> ToolDefinition {
        let available = eval::list_eval_files(&self.memory_dir);
        let evals_list = if available.is_empty() {
            "No eval files found. Use generate_evals first.".to_string()
        } else {
            format!("Available evals: {}", available.join(", "))
        };

        ToolDefinition {
            name: "run_evals".to_string(),
            description: format!(
                "Run evaluation test cases for sub-agents. \
                Without agent_name — runs all available evals. \
                With agent_name — runs evals for that specific sub-agent. \
                With eval_id — runs only that specific eval case. {}",
                evals_list
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_name": {
                        "type": "string",
                        "description": "Name of the sub-agent to run evals for. If omitted, runs all available evals."
                    },
                    "eval_id": {
                        "type": "string",
                        "description": "Run only a specific eval case by its ID."
                    }
                },
                "required": []
            }),
        }
    }

    async fn execute(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid arguments: {e}"))?;

        let agent_name = args
            .get("agent_name")
            .and_then(|v| v.as_str())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty());

        let eval_id = args
            .get("eval_id")
            .and_then(|v| v.as_str())
            .map(|s| s.trim())
            .filter(|s| !s.is_empty());

        let agent_names = if let Some(name) = agent_name {
            if name.contains("..") || name.contains('/') || name.contains('\\') {
                return Err("Invalid agent name: path traversal is not allowed.".to_string());
            }
            vec![name.to_string()]
        } else {
            let names = eval::list_eval_files(&self.memory_dir);
            if names.is_empty() {
                return Ok(
                    "No eval files found. Use generate_evals to create them first.".to_string(),
                );
            }
            names
        };

        let mut full_report = String::new();
        let mut total_passed = 0;
        let mut total_failed = 0;

        for name in &agent_names {
            let suite = eval::load_eval_suite(&self.memory_dir, name)?;

            log::info!("Running eval suite for '{}'", name);

            let report =
                eval::run_eval_suite(self.provider.clone(), &self.memory_dir, &suite, eval_id)
                    .await;

            total_passed += report.passed;
            total_failed += report.failed;

            if let Err(e) = eval::save_eval_report(&self.memory_dir, &report).await {
                log::warn!("Failed to save eval report: {e}");
            }

            full_report.push_str(&eval::format_report(&report));
        }

        if agent_names.len() > 1 {
            full_report.push_str(&format!(
                "=== Overall: {} passed, {} failed ===\n",
                total_passed, total_failed
            ));
        }

        Ok(full_report)
    }
}
