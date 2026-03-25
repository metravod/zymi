use std::time::Duration;

use async_trait::async_trait;
use rmcp::model::{CallToolRequestParams, RawContent};
use rmcp::service::Peer;
use rmcp::service::RoleClient;

use crate::core::ToolDefinition;
use crate::tools::Tool;

/// Maximum time an MCP tool call may take before being cancelled.
const MCP_CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Maximum response text length (characters). Responses exceeding this are truncated.
const MCP_MAX_RESPONSE_LEN: usize = 100_000;

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

        let result = tokio::time::timeout(
            MCP_CALL_TIMEOUT,
            self.peer.call_tool(CallToolRequestParams {
                meta: None,
                name: self.original_name.clone().into(),
                arguments: args,
                task: None,
            }),
        )
        .await
        .map_err(|_| {
            format!(
                "MCP tool '{}' timed out after {}s",
                self.original_name,
                MCP_CALL_TIMEOUT.as_secs()
            )
        })?
        .map_err(|e| format!("MCP call_tool error: {e}"))?;

        if result.is_error == Some(true) {
            let text = extract_text(&result.content);
            return Err(truncate_response(text));
        }

        Ok(truncate_response(extract_text(&result.content)))
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

fn truncate_response(s: String) -> String {
    if s.len() <= MCP_MAX_RESPONSE_LEN {
        s
    } else {
        let end = s.floor_char_boundary(MCP_MAX_RESPONSE_LEN);
        format!(
            "{}...\n[MCP response truncated: {} total chars, limit {}]",
            &s[..end],
            s.len(),
            MCP_MAX_RESPONSE_LEN
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_response_short() {
        let s = "hello world".to_string();
        assert_eq!(truncate_response(s.clone()), s);
    }

    #[test]
    fn truncate_response_at_limit() {
        let s = "a".repeat(MCP_MAX_RESPONSE_LEN);
        assert_eq!(truncate_response(s.clone()), s);
    }

    #[test]
    fn truncate_response_over_limit() {
        let s = "a".repeat(MCP_MAX_RESPONSE_LEN + 500);
        let result = truncate_response(s);
        assert!(result.contains("[MCP response truncated:"));
        assert!(result.len() < MCP_MAX_RESPONSE_LEN + 200);
    }
}
