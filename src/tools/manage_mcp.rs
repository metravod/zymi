use std::collections::HashMap;
use std::path::PathBuf;

use async_trait::async_trait;

use crate::core::ToolDefinition;
use crate::tools::Tool;

pub struct ManageMcpTool {
    memory_dir: PathBuf,
}

impl ManageMcpTool {
    pub fn new(memory_dir: PathBuf) -> Self {
        Self { memory_dir }
    }

    fn mcp_path(&self) -> PathBuf {
        self.memory_dir.join("mcp.json")
    }

    fn load_config(&self) -> serde_json::Value {
        match std::fs::read_to_string(self.mcp_path()) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_else(|_| {
                serde_json::json!({"mcpServers": {}})
            }),
            Err(_) => serde_json::json!({"mcpServers": {}}),
        }
    }

    fn save_config(&self, config: &serde_json::Value) -> Result<(), String> {
        let path = self.mcp_path();
        let tmp = self.memory_dir.join("mcp.json.tmp");
        let content =
            serde_json::to_string_pretty(config).map_err(|e| format!("Serialize error: {e}"))?;
        std::fs::write(&tmp, &content).map_err(|e| format!("Write error: {e}"))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("Rename error: {e}"))?;
        Ok(())
    }

    fn handle_list(&self) -> Result<String, String> {
        let config = self.load_config();
        let servers = config["mcpServers"]
            .as_object()
            .cloned()
            .unwrap_or_default();

        if servers.is_empty() {
            return Ok("No MCP servers configured.".to_string());
        }

        let mut lines = vec![format!("MCP servers ({}):", servers.len())];
        for (name, cfg) in &servers {
            if let Some(cmd) = cfg["command"].as_str() {
                let args = cfg["args"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                lines.push(format!("  {name}: {cmd} {args}"));
            } else if let Some(url) = cfg["url"].as_str() {
                lines.push(format!("  {name}: {url}"));
            } else {
                lines.push(format!("  {name}: (invalid config)"));
            }
        }
        Ok(lines.join("\n"))
    }

    fn handle_add(&self, args: &serde_json::Value) -> Result<String, String> {
        let name = args["name"]
            .as_str()
            .ok_or("Missing 'name' for MCP server")?;

        // Validate name
        if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains(' ') {
            return Err("Invalid server name (no spaces, slashes)".to_string());
        }

        let mut config = self.load_config();
        let servers = config["mcpServers"]
            .as_object_mut()
            .ok_or("Invalid mcp.json structure")?;

        // Build server entry
        let mut entry = serde_json::Map::new();

        if let Some(url) = args["url"].as_str() {
            entry.insert("url".into(), serde_json::Value::String(url.to_string()));
        } else {
            let command = args["command"]
                .as_str()
                .ok_or("Either 'command' or 'url' is required")?;
            entry.insert(
                "command".into(),
                serde_json::Value::String(command.to_string()),
            );

            if let Some(cmd_args) = args["args"].as_array() {
                entry.insert("args".into(), serde_json::Value::Array(cmd_args.clone()));
            }

            let env: HashMap<String, String> = args["env"]
                .as_object()
                .map(|o| {
                    o.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            if !env.is_empty() {
                entry.insert("env".into(), serde_json::to_value(env).unwrap());
            }
        }

        let is_update = servers.contains_key(name);
        servers.insert(name.to_string(), serde_json::Value::Object(entry));
        self.save_config(&config)?;

        let verb = if is_update { "Updated" } else { "Added" };
        Ok(format!(
            "{verb} MCP server '{name}'. Hot-reloading..."
        ))
    }

    fn handle_remove(&self, args: &serde_json::Value) -> Result<String, String> {
        let name = args["name"]
            .as_str()
            .ok_or("Missing 'name' for MCP server to remove")?;

        let mut config = self.load_config();
        let servers = config["mcpServers"]
            .as_object_mut()
            .ok_or("Invalid mcp.json structure")?;

        if servers.remove(name).is_none() {
            return Err(format!("MCP server '{name}' not found"));
        }

        self.save_config(&config)?;
        Ok(format!(
            "Removed MCP server '{name}'. Restart daemon to fully disconnect."
        ))
    }
}

#[async_trait]
impl Tool for ManageMcpTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "manage_mcp".to_string(),
            description: "Manage MCP (Model Context Protocol) servers. Actions: 'list' (show configured servers), \
                'add' (add/update a server — new servers are hot-reloaded immediately), \
                'remove' (remove a server). \
                To add an MCP server: first install it (npm/pip/docker) with execute_shell if needed, \
                then use this tool to register it."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list", "add", "remove"],
                        "description": "Action to perform"
                    },
                    "name": {
                        "type": "string",
                        "description": "Server name (for 'add'/'remove')"
                    },
                    "command": {
                        "type": "string",
                        "description": "Command to start the server (for 'add'), e.g. 'npx', 'docker', 'uvx'"
                    },
                    "args": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Command arguments (for 'add'), e.g. ['-y', '@modelcontextprotocol/server-docker']"
                    },
                    "env": {
                        "type": "object",
                        "description": "Environment variables for the server process (for 'add')"
                    },
                    "url": {
                        "type": "string",
                        "description": "HTTP/SSE endpoint URL (for 'add', alternative to command)"
                    }
                },
                "required": ["action"]
            }),
        }
    }

    async fn execute(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid arguments: {e}"))?;

        let action = args["action"]
            .as_str()
            .ok_or("Missing required parameter: action")?;

        match action {
            "list" => self.handle_list(),
            "add" => self.handle_add(&args),
            "remove" => self.handle_remove(&args),
            _ => Err(format!(
                "Unknown action: {action}. Use 'list', 'add', or 'remove'."
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tool(dir: &std::path::Path) -> ManageMcpTool {
        ManageMcpTool::new(dir.to_path_buf())
    }

    #[tokio::test]
    async fn handle_list_empty() {
        let dir = tempfile::tempdir().unwrap();
        let tool = make_tool(dir.path());
        let result = tool.execute(r#"{"action":"list"}"#).await.unwrap();
        assert_eq!(result, "No MCP servers configured.");
    }

    #[tokio::test]
    async fn handle_list_with_servers() {
        let dir = tempfile::tempdir().unwrap();
        let config = serde_json::json!({
            "mcpServers": {
                "github": {"command": "npx", "args": ["-y", "@mcp/github"]},
                "slack": {"url": "http://localhost:3001"}
            }
        });
        std::fs::write(
            dir.path().join("mcp.json"),
            serde_json::to_string_pretty(&config).unwrap(),
        )
        .unwrap();

        let tool = make_tool(dir.path());
        let result = tool.execute(r#"{"action":"list"}"#).await.unwrap();
        assert!(result.contains("MCP servers (2):"));
        assert!(result.contains("github:"));
        assert!(result.contains("slack:"));
    }

    #[tokio::test]
    async fn handle_add_command_server() {
        let dir = tempfile::tempdir().unwrap();
        let tool = make_tool(dir.path());
        let args = serde_json::json!({
            "action": "add",
            "name": "github",
            "command": "npx",
            "args": ["-y", "@mcp/github"]
        });
        let result = tool.execute(&args.to_string()).await.unwrap();
        assert!(result.contains("Added"));
        assert!(result.contains("github"));

        // Verify file content
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("mcp.json")).unwrap())
                .unwrap();
        assert_eq!(config["mcpServers"]["github"]["command"], "npx");
    }

    #[tokio::test]
    async fn handle_add_url_server() {
        let dir = tempfile::tempdir().unwrap();
        let tool = make_tool(dir.path());
        let args = serde_json::json!({
            "action": "add",
            "name": "remote",
            "url": "http://localhost:3000/mcp"
        });
        let result = tool.execute(&args.to_string()).await.unwrap();
        assert!(result.contains("Added"));

        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("mcp.json")).unwrap())
                .unwrap();
        assert_eq!(
            config["mcpServers"]["remote"]["url"],
            "http://localhost:3000/mcp"
        );
    }

    #[tokio::test]
    async fn handle_add_with_env() {
        let dir = tempfile::tempdir().unwrap();
        let tool = make_tool(dir.path());
        let args = serde_json::json!({
            "action": "add",
            "name": "myserver",
            "command": "uvx",
            "env": {"API_KEY": "secret123"}
        });
        let result = tool.execute(&args.to_string()).await.unwrap();
        assert!(result.contains("Added"));

        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("mcp.json")).unwrap())
                .unwrap();
        assert_eq!(
            config["mcpServers"]["myserver"]["env"]["API_KEY"],
            "secret123"
        );
    }

    #[tokio::test]
    async fn handle_add_updates_existing() {
        let dir = tempfile::tempdir().unwrap();
        let tool = make_tool(dir.path());

        // Add first
        let args = serde_json::json!({"action":"add","name":"srv","command":"old"});
        tool.execute(&args.to_string()).await.unwrap();

        // Update
        let args = serde_json::json!({"action":"add","name":"srv","command":"new"});
        let result = tool.execute(&args.to_string()).await.unwrap();
        assert!(result.contains("Updated"));

        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("mcp.json")).unwrap())
                .unwrap();
        assert_eq!(config["mcpServers"]["srv"]["command"], "new");
    }

    #[tokio::test]
    async fn handle_add_missing_command_and_url() {
        let dir = tempfile::tempdir().unwrap();
        let tool = make_tool(dir.path());
        let args = serde_json::json!({"action":"add","name":"srv"});
        let result = tool.execute(&args.to_string()).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("command"));
    }

    #[tokio::test]
    async fn handle_add_invalid_name() {
        let dir = tempfile::tempdir().unwrap();
        let tool = make_tool(dir.path());

        for name in &["", "a/b", "a\\b", "a b"] {
            let args = serde_json::json!({"action":"add","name":name,"command":"x"});
            let result = tool.execute(&args.to_string()).await;
            assert!(result.is_err(), "Expected error for name '{name}'");
        }
    }

    #[tokio::test]
    async fn handle_remove_existing() {
        let dir = tempfile::tempdir().unwrap();
        let tool = make_tool(dir.path());

        // Add then remove
        let add = serde_json::json!({"action":"add","name":"srv","command":"x"});
        tool.execute(&add.to_string()).await.unwrap();

        let rm = serde_json::json!({"action":"remove","name":"srv"});
        let result = tool.execute(&rm.to_string()).await.unwrap();
        assert!(result.contains("Removed"));

        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("mcp.json")).unwrap())
                .unwrap();
        assert!(config["mcpServers"].as_object().unwrap().is_empty());
    }

    #[tokio::test]
    async fn handle_remove_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let tool = make_tool(dir.path());
        let args = serde_json::json!({"action":"remove","name":"nope"});
        let result = tool.execute(&args.to_string()).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[tokio::test]
    async fn execute_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let tool = make_tool(dir.path());
        let result = tool.execute("not json").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn execute_unknown_action() {
        let dir = tempfile::tempdir().unwrap();
        let tool = make_tool(dir.path());
        let result = tool.execute(r#"{"action":"nope"}"#).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unknown action"));
    }
}
