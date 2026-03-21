use std::path::PathBuf;

use async_trait::async_trait;

use crate::core::ToolDefinition;
use crate::policy::{self, PolicyConfig};
use crate::tools::Tool;

pub struct ManagePolicyTool {
    memory_dir: PathBuf,
}

impl ManagePolicyTool {
    pub fn new(memory_dir: PathBuf) -> Self {
        Self { memory_dir }
    }

    fn handle_status(&self) -> Result<String, String> {
        let config = policy::load_policy(&self.memory_dir);

        let mut lines = vec![format!("Policy engine: {}", if config.enabled { "enabled" } else { "disabled" })];

        if !config.allow.is_empty() {
            lines.push(format!("  allow: {:?}", config.allow));
        } else {
            lines.push("  allow: (none)".to_string());
        }

        if !config.deny.is_empty() {
            lines.push(format!("  deny: {:?}", config.deny));
        } else {
            lines.push("  deny: (none)".to_string());
        }

        if !config.require_approval.is_empty() {
            lines.push(format!("  require_approval: {:?}", config.require_approval));
        } else {
            lines.push("  require_approval: (none)".to_string());
        }

        lines.push(String::new());
        lines.push("Note: Changes take effect on next daemon restart.".to_string());

        Ok(lines.join("\n"))
    }

    fn handle_enable(&self) -> Result<String, String> {
        let mut config = policy::load_policy(&self.memory_dir);
        config.enabled = true;
        self.save_config(&config)?;
        Ok("Policy engine enabled. Restart daemon to apply.".to_string())
    }

    fn handle_disable(&self) -> Result<String, String> {
        let mut config = policy::load_policy(&self.memory_dir);
        config.enabled = false;
        self.save_config(&config)?;
        Ok("Policy engine disabled. Restart daemon to apply.".to_string())
    }

    fn handle_add_rule(&self, args: &serde_json::Value) -> Result<String, String> {
        let list = args
            .get("list")
            .and_then(|v| v.as_str())
            .ok_or("Missing 'list' parameter (allow/deny/require_approval)")?;

        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or("Missing 'pattern' parameter (glob pattern)")?;

        let mut config = policy::load_policy(&self.memory_dir);

        let target = match list {
            "allow" => &mut config.allow,
            "deny" => &mut config.deny,
            "require_approval" => &mut config.require_approval,
            _ => return Err(format!("Unknown list: {list}. Use allow/deny/require_approval.")),
        };

        if target.contains(&pattern.to_string()) {
            return Err(format!("Pattern '{pattern}' already in {list} list."));
        }

        target.push(pattern.to_string());
        self.save_config(&config)?;

        Ok(format!("Added '{pattern}' to {list} list."))
    }

    fn handle_remove_rule(&self, args: &serde_json::Value) -> Result<String, String> {
        let list = args
            .get("list")
            .and_then(|v| v.as_str())
            .ok_or("Missing 'list' parameter (allow/deny/require_approval)")?;

        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .ok_or("Missing 'pattern' parameter")?;

        let mut config = policy::load_policy(&self.memory_dir);

        let target = match list {
            "allow" => &mut config.allow,
            "deny" => &mut config.deny,
            "require_approval" => &mut config.require_approval,
            _ => return Err(format!("Unknown list: {list}. Use allow/deny/require_approval.")),
        };

        let before = target.len();
        target.retain(|p| p != pattern);

        if target.len() == before {
            return Err(format!("Pattern '{pattern}' not found in {list} list."));
        }

        self.save_config(&config)?;

        Ok(format!("Removed '{pattern}' from {list} list."))
    }

    fn save_config(&self, config: &PolicyConfig) -> Result<(), String> {
        let path = self.memory_dir.join("policy.json");
        let tmp = self.memory_dir.join("policy.json.tmp");
        let content =
            serde_json::to_string_pretty(config).map_err(|e| format!("Serialize error: {e}"))?;
        std::fs::write(&tmp, &content).map_err(|e| format!("Write error: {e}"))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("Rename error: {e}"))?;
        Ok(())
    }
}

#[async_trait]
impl Tool for ManagePolicyTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "manage_policy".to_string(),
            description: "Manage shell command policy engine. Actions: 'status' (show rules), \
                'enable'/'disable' (toggle engine), 'add_rule' (add glob pattern to allow/deny/require_approval list), \
                'remove_rule' (remove pattern). Policy controls which shell commands the agent can auto-execute \
                vs those requiring human approval or outright denied."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["status", "enable", "disable", "add_rule", "remove_rule"],
                        "description": "Action to perform"
                    },
                    "list": {
                        "type": "string",
                        "enum": ["allow", "deny", "require_approval"],
                        "description": "Which rule list to modify (for add_rule/remove_rule)"
                    },
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern for the rule (e.g. 'docker ps *', 'rm -rf *')"
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
            "status" => self.handle_status(),
            "enable" => self.handle_enable(),
            "disable" => self.handle_disable(),
            "add_rule" => self.handle_add_rule(&args),
            "remove_rule" => self.handle_remove_rule(&args),
            _ => Err(format!(
                "Unknown action: {action}. Use 'status', 'enable', 'disable', 'add_rule', or 'remove_rule'."
            )),
        }
    }
}
