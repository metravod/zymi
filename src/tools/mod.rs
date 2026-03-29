pub mod ask_user;
pub mod create_sub_agent;
pub mod current_time;
pub mod eval_gen;
pub mod eval_run;
pub mod manage_mcp;
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
