use std::sync::Arc;

use async_trait::async_trait;

use crate::core::ToolDefinition;
use crate::skills::SkillManager;
use crate::tools::Tool;

pub struct ManageSkillsTool {
    skill_manager: Arc<SkillManager>,
}

impl ManageSkillsTool {
    pub fn new(skill_manager: Arc<SkillManager>) -> Self {
        Self { skill_manager }
    }
}

#[async_trait]
impl Tool for ManageSkillsTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "manage_skills".to_string(),
            description: "Manage skills — community-contributed extensions installed from git repos. \
                Skills add knowledge (injected into context when relevant) and/or MCP tool servers. \
                Actions: 'list' (show installed skills), \
                'install' (install from git repo URL, optionally specify sub-path for monorepos), \
                'update' (pull latest for a skill), \
                'remove' (uninstall a skill), \
                'search' (search installed skills by keyword)."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list", "install", "update", "remove", "search"],
                        "description": "Action to perform"
                    },
                    "repo": {
                        "type": "string",
                        "description": "Git repository URL (for 'install'), e.g. 'https://github.com/user/skills-repo'"
                    },
                    "path": {
                        "type": "string",
                        "description": "Sub-path within repo for monorepos (for 'install'), e.g. 'plugins/my-skill'"
                    },
                    "name": {
                        "type": "string",
                        "description": "Skill name (for 'update'/'remove')"
                    },
                    "query": {
                        "type": "string",
                        "description": "Search query (for 'search')"
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
            "list" => Ok(self.skill_manager.list().await),

            "install" => {
                let repo = args["repo"]
                    .as_str()
                    .ok_or("Missing 'repo' for install action")?;
                let sub_path = args["path"].as_str();
                self.skill_manager.install(repo, sub_path).await
            }

            "update" => {
                let name = args["name"]
                    .as_str()
                    .ok_or("Missing 'name' for update action")?;
                self.skill_manager.update(name).await
            }

            "remove" => {
                let name = args["name"]
                    .as_str()
                    .ok_or("Missing 'name' for remove action")?;
                self.skill_manager.remove(name).await
            }

            "search" => {
                let query = args["query"]
                    .as_str()
                    .ok_or("Missing 'query' for search action")?;
                let matched = self.skill_manager.match_skills(query).await;
                if matched.is_empty() {
                    Ok("No matching skills found.".to_string())
                } else {
                    let mut lines = vec![format!("Matching skills ({}):", matched.len())];
                    for s in &matched {
                        let version = s.version.as_deref().unwrap_or("?");
                        lines.push(format!("  {} v{} — {}", s.name, version, s.description));
                    }
                    Ok(lines.join("\n"))
                }
            }

            _ => Err(format!(
                "Unknown action: {action}. Use 'list', 'install', 'update', 'remove', or 'search'."
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_manager(dir: &std::path::Path) -> Arc<SkillManager> {
        Arc::new(SkillManager::init(dir))
    }

    #[tokio::test]
    async fn list_empty() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ManageSkillsTool::new(setup_manager(dir.path()));
        let result = tool.execute(r#"{"action":"list"}"#).await.unwrap();
        assert_eq!(result, "No skills installed.");
    }

    #[tokio::test]
    async fn install_missing_repo() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ManageSkillsTool::new(setup_manager(dir.path()));
        let result = tool.execute(r#"{"action":"install"}"#).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("repo"));
    }

    #[tokio::test]
    async fn update_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ManageSkillsTool::new(setup_manager(dir.path()));
        let result = tool
            .execute(r#"{"action":"update","name":"nope"}"#)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not installed"));
    }

    #[tokio::test]
    async fn remove_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ManageSkillsTool::new(setup_manager(dir.path()));
        let result = tool
            .execute(r#"{"action":"remove","name":"nope"}"#)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn search_empty() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ManageSkillsTool::new(setup_manager(dir.path()));
        let result = tool
            .execute(r#"{"action":"search","query":"web"}"#)
            .await
            .unwrap();
        assert_eq!(result, "No matching skills found.");
    }

    #[tokio::test]
    async fn unknown_action() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ManageSkillsTool::new(setup_manager(dir.path()));
        let result = tool.execute(r#"{"action":"foo"}"#).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unknown action"));
    }

    #[tokio::test]
    async fn invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ManageSkillsTool::new(setup_manager(dir.path()));
        let result = tool.execute("not json").await;
        assert!(result.is_err());
    }
}
