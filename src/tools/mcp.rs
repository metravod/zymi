use async_trait::async_trait;
use rmcp::model::{CallToolRequestParams, RawContent};
use rmcp::service::Peer;
use rmcp::service::RoleClient;

use crate::core::ToolDefinition;
use crate::tools::Tool;

pub struct McpTool {
    prefixed_name: String,
    original_name: String,
    description: String,
    input_schema: serde_json::Value,
    peer: Peer<RoleClient>,
}

impl McpTool {
    pub fn new(
        server_name: &str,
        tool: &rmcp::model::Tool,
        peer: Peer<RoleClient>,
    ) -> Self {
        let original_name = tool.name.to_string();
        let prefixed_name = format!("{server_name}_{original_name}");
        let description = tool
            .description
            .as_deref()
            .unwrap_or("No description")
            .to_string();
        let input_schema =
            serde_json::to_value(tool.input_schema.as_ref()).unwrap_or(serde_json::json!({}));

        Self {
            prefixed_name,
            original_name,
            description,
            input_schema,
            peer,
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.prefixed_name.clone(),
            description: format!("[MCP] {}", self.description),
            parameters: self.input_schema.clone(),
        }
    }

    async fn execute(&self, arguments: &str) -> Result<String, String> {
        let args: Option<serde_json::Map<String, serde_json::Value>> = if arguments.is_empty() {
            None
        } else {
            serde_json::from_str(arguments).map_err(|e| format!("Invalid JSON arguments: {e}"))?
        };

        let result = self
            .peer
            .call_tool(CallToolRequestParams {
                meta: None,
                name: self.original_name.clone().into(),
                arguments: args,
                task: None,
            })
            .await
            .map_err(|e| format!("MCP call_tool error: {e}"))?;

        if result.is_error == Some(true) {
            let text = extract_text(&result.content);
            return Err(text);
        }

        Ok(extract_text(&result.content))
    }

    fn requires_approval(&self) -> bool {
        false
    }
}

fn extract_text(content: &[rmcp::model::Content]) -> String {
    content
        .iter()
        .filter_map(|c| match &c.raw {
            RawContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
