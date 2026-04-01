pub mod ask_user;
pub mod create_sub_agent;
pub mod current_time;
pub mod eval_gen;
pub mod eval_run;
pub mod manage_mcp;
pub mod manage_skills;
pub mod mcp;
pub mod memory;
pub mod planning;
pub mod policy;
pub mod run_code;
pub mod schedule;
pub mod shell;
pub mod sub_agent;
pub mod task;
pub mod web_scrape;
pub mod web_search;
pub mod youtube_transcript;

use async_trait::async_trait;

use crate::core::ToolDefinition;
use crate::esaa::Intention;

#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    async fn execute(&self, arguments: &str) -> Result<String, String>;

    /// Return a rich, contextual prompt that guides the LLM on how to use this tool.
    /// Merged into the tool description sent to the API. Use this for behavioral
    /// guidelines, safety constraints, and when-to-use-vs-alternatives guidance.
    fn prompt(&self) -> Option<String> {
        None
    }

    /// Whether this tool only reads data and has no side effects.
    /// Read-only tools skip approval in both ESAA and legacy paths.
    fn is_read_only(&self) -> bool {
        false
    }

    /// Whether this tool performs destructive or hard-to-reverse operations.
    /// Destructive tools always require approval, even if auto-approve is enabled.
    fn is_destructive(&self) -> bool {
        false
    }

    fn requires_approval(&self) -> bool {
        false
    }

    /// Check if approval is required for specific arguments.
    /// Override this for tools with policy engines that can auto-approve certain calls.
    fn requires_approval_for(&self, _arguments: &str) -> bool {
        self.requires_approval()
    }

    fn format_approval_request(&self, arguments: &str) -> String {
        format!("Tool: {}\nArguments: {}", self.definition().name, arguments)
    }

    /// Convert a tool call into an ESAA Intention for orchestrator evaluation.
    /// Returns None for tools that haven't been migrated to the intention model yet.
    fn to_intention(&self, _arguments: &str) -> Option<Intention> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use current_time::CurrentTimeTool;

    // -- trait default tests --

    struct DummyTool;
    #[async_trait]
    impl Tool for DummyTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "dummy".into(),
                description: "test".into(),
                parameters: serde_json::json!({}),
            }
        }
        async fn execute(&self, _args: &str) -> Result<String, String> {
            Ok("ok".into())
        }
    }

    #[test]
    fn defaults_are_conservative() {
        let t = DummyTool;
        assert!(!t.is_read_only(), "default is_read_only should be false");
        assert!(!t.is_destructive(), "default is_destructive should be false");
        assert!(!t.requires_approval(), "default requires_approval should be false");
    }

    #[test]
    fn read_only_and_destructive_are_mutually_exclusive_on_real_tools() {
        // CurrentTimeTool is read-only
        let t = CurrentTimeTool;
        assert!(t.is_read_only());
        assert!(!t.is_destructive());
    }

    #[test]
    fn current_time_is_read_only() {
        assert!(CurrentTimeTool.is_read_only());
        assert!(!CurrentTimeTool.requires_approval());
    }
}
