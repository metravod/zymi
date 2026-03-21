use async_trait::async_trait;
use chrono::Utc;

use crate::core::ToolDefinition;
use crate::tools::Tool;

pub struct CurrentTimeTool;

#[async_trait]
impl Tool for CurrentTimeTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "get_current_time".to_string(),
            description: "Returns the current date and time in UTC".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        }
    }

    async fn execute(&self, _arguments: &str) -> Result<String, String> {
        Ok(Utc::now().format("%Y-%m-%d %H:%M:%S UTC").to_string())
    }
}
