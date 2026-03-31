pub mod loader;
pub mod matcher;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use loader::SkillMeta;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub name: String,
    pub repo: String,
    /// Sub-path within the repo (for monorepos), e.g. "plugins/my-skill".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub installed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Registry {
    skills: Vec<RegistryEntry>,
}

/// A fully loaded skill: registry metadata + parsed SKILL.md entries.
pub struct InstalledSkill {
    pub entry: RegistryEntry,
    pub local_dir: PathBuf,
    pub skills: Vec<SkillMeta>,
    pub mcp_config: Option<serde_json::Value>,
}

pub struct SkillManager {
    skills_dir: PathBuf,
    installed: RwLock<Vec<InstalledSkill>>,
}

impl SkillManager {
    /// Initialize SkillManager: read registry, scan installed skill directories.
    pub fn init(memory_dir: &Path) -> Self {
        let skills_dir = memory_dir.join("skills");
        if !skills_dir.exists() {
            let _ = std::fs::create_dir_all(&skills_dir);
        }

        let registry = load_registry(&skills_dir);
        let mut installed = Vec::new();

        for entry in registry.skills {
            let local_dir = resolve_local_dir(&skills_dir, &entry);
            if !local_dir.exists() {
                log::warn!("Skill '{}' directory missing: {}", entry.name, local_dir.display());
                continue;
            }

            let skill_scan_dir = match &entry.path {
                Some(sub) => local_dir.join(sub),
                None => local_dir.clone(),
            };

            let skills = loader::discover_skills(&skill_scan_dir);
            let mcp_config = loader::discover_mcp(&skill_scan_dir);

            if skills.is_empty() && mcp_config.is_none() {
                log::warn!("Skill '{}': no SKILL.md or .mcp.json found", entry.name);
            } else {
                log::info!(
                    "Skill '{}': {} knowledge skills{}",
                    entry.name,
                    skills.len(),
                    if mcp_config.is_some() { " + MCP" } else { "" }
                );
            }

            installed.push(InstalledSkill {
                entry,
                local_dir,
                skills,
                mcp_config,
            });
        }

        Self {
            skills_dir,
            installed: RwLock::new(installed),
        }
    }

    /// Install a skill from a git repository.
    pub async fn install(
        &self,
        repo_url: &str,
        sub_path: Option<&str>,
    ) -> Result<String, String> {
        let dir_name = loader::repo_dir_name(repo_url);

        // Check if already installed
        {
            let installed = self.installed.read().await;
            if installed.iter().any(|s| s.entry.name == dir_name) {
                return Err(format!("Skill '{dir_name}' is already installed. Use update to refresh."));
            }
        }

        let target_dir = self.skills_dir.join(&dir_name);
        if target_dir.exists() {
            std::fs::remove_dir_all(&target_dir)
                .map_err(|e| format!("Failed to clean existing directory: {e}"))?;
        }

        loader::git_clone(repo_url, &target_dir).await?;

        let skill_scan_dir = match sub_path {
            Some(sub) => target_dir.join(sub),
            None => target_dir.clone(),
        };

        let skills = loader::discover_skills(&skill_scan_dir);
        let mcp_config = loader::discover_mcp(&skill_scan_dir);

        if skills.is_empty() && mcp_config.is_none() {
            // Clean up
            let _ = std::fs::remove_dir_all(&target_dir);
            return Err("No SKILL.md or .mcp.json found in repository.".to_string());
        }

        let version = skills.first().and_then(|s| s.version.clone());
        let skill_names: Vec<String> = skills.iter().map(|s| s.name.clone()).collect();

        let entry = RegistryEntry {
            name: dir_name.clone(),
            repo: repo_url.to_string(),
            path: sub_path.map(String::from),
            installed_at: chrono::Utc::now().to_rfc3339(),
            version,
        };

        let mut installed = self.installed.write().await;
        installed.push(InstalledSkill {
            entry: entry.clone(),
            local_dir: target_dir,
            skills,
            mcp_config,
        });

        self.save_registry_from(&installed);

        let mut result = format!("Installed '{dir_name}'.");
        if !skill_names.is_empty() {
            result.push_str(&format!(" Skills: [{}].", skill_names.join(", ")));
        }
        Ok(result)
    }

    /// Update an installed skill (git pull + re-scan).
    pub async fn update(&self, name: &str) -> Result<String, String> {
        let mut installed = self.installed.write().await;
        let idx = installed
            .iter()
            .position(|s| s.entry.name == name)
            .ok_or_else(|| format!("Skill '{name}' is not installed."))?;

        let repo_dir = &installed[idx].local_dir;
        loader::git_pull(repo_dir).await?;

        let skill_scan_dir = match &installed[idx].entry.path {
            Some(sub) => repo_dir.join(sub),
            None => repo_dir.clone(),
        };

        let skills = loader::discover_skills(&skill_scan_dir);
        let mcp_config = loader::discover_mcp(&skill_scan_dir);

        let version = skills.first().and_then(|s| s.version.clone());
        installed[idx].skills = skills;
        installed[idx].mcp_config = mcp_config;
        installed[idx].entry.version = version;

        self.save_registry_from(&installed);

        Ok(format!("Updated '{name}' to latest."))
    }

    /// Remove an installed skill.
    pub async fn remove(&self, name: &str) -> Result<String, String> {
        let mut installed = self.installed.write().await;
        let idx = installed
            .iter()
            .position(|s| s.entry.name == name)
            .ok_or_else(|| format!("Skill '{name}' is not installed."))?;

        let removed = installed.remove(idx);
        if removed.local_dir.exists() {
            std::fs::remove_dir_all(&removed.local_dir)
                .map_err(|e| format!("Failed to remove directory: {e}"))?;
        }

        self.save_registry_from(&installed);

        Ok(format!("Removed '{name}'."))
    }

    /// List installed skills.
    pub async fn list(&self) -> String {
        let installed = self.installed.read().await;
        if installed.is_empty() {
            return "No skills installed.".to_string();
        }

        let mut lines = vec![format!("Installed skills ({}):", installed.len())];
        for s in installed.iter() {
            let skill_names: Vec<&str> = s.skills.iter().map(|sk| sk.name.as_str()).collect();
            let version = s.entry.version.as_deref().unwrap_or("?");
            let mcp_flag = if s.mcp_config.is_some() { " +MCP" } else { "" };
            lines.push(format!(
                "  {} v{} — [{}]{} ({})",
                s.entry.name,
                version,
                skill_names.join(", "),
                mcp_flag,
                s.entry.repo,
            ));
        }
        lines.join("\n")
    }

    /// Find skills matching a user message by keyword relevance.
    pub async fn match_skills(&self, user_message: &str) -> Vec<SkillMeta> {
        let installed = self.installed.read().await;
        let all_skills: Vec<&SkillMeta> = installed
            .iter()
            .flat_map(|s| s.skills.iter())
            .collect();

        // Collect into owned vec since we need to return across the lock boundary
        let all_owned: Vec<SkillMeta> = all_skills.into_iter().cloned().collect();
        let matched = matcher::match_skills(&all_owned, user_message);
        matched.into_iter().cloned().collect()
    }

    /// Get MCP configs from all installed skills that have them.
    /// Used during init to register skill-provided MCP servers.
    #[allow(dead_code)]
    pub async fn mcp_configs(&self) -> Vec<(String, serde_json::Value)> {
        let installed = self.installed.read().await;
        installed
            .iter()
            .filter_map(|s| {
                s.mcp_config
                    .as_ref()
                    .map(|cfg| (s.entry.name.clone(), cfg.clone()))
            })
            .collect()
    }

    fn save_registry_from(&self, installed: &[InstalledSkill]) {
        let registry = Registry {
            skills: installed.iter().map(|s| s.entry.clone()).collect(),
        };
        save_registry(&self.skills_dir, &registry);
    }
}

/// Resolve the local clone directory for a registry entry.
fn resolve_local_dir(skills_dir: &Path, entry: &RegistryEntry) -> PathBuf {
    skills_dir.join(&entry.name)
}

fn registry_path(skills_dir: &Path) -> PathBuf {
    skills_dir.join("registry.json")
}

fn load_registry(skills_dir: &Path) -> Registry {
    let path = registry_path(skills_dir);
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => Registry::default(),
    }
}

fn save_registry(skills_dir: &Path, registry: &Registry) {
    let path = registry_path(skills_dir);
    match serde_json::to_string_pretty(registry) {
        Ok(content) => {
            if let Err(e) = std::fs::write(&path, content) {
                log::warn!("Failed to save skills registry: {e}");
            }
        }
        Err(e) => log::warn!("Failed to serialize skills registry: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_skill_dir(dir: &Path, name: &str, desc: &str) {
        let skill_dir = dir.join("skills").join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {desc}\n---\n\nSkill body for {name}.\n"),
        )
        .unwrap();
    }

    #[test]
    fn init_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let sm = SkillManager::init(dir.path());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let list = rt.block_on(sm.list());
        assert_eq!(list, "No skills installed.");
    }

    #[test]
    fn init_with_existing_skills() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();

        // Create a "plugin" directory with a SKILL.md
        let plugin_dir = skills_dir.join("test-plugin");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        setup_skill_dir(&plugin_dir, "test-skill", "A test skill for testing");

        // Write registry
        let registry = Registry {
            skills: vec![RegistryEntry {
                name: "test-plugin".to_string(),
                repo: "https://github.com/test/test-plugin".to_string(),
                path: None,
                installed_at: "2026-03-30T00:00:00Z".to_string(),
                version: None,
            }],
        };
        std::fs::write(
            skills_dir.join("registry.json"),
            serde_json::to_string_pretty(&registry).unwrap(),
        )
        .unwrap();

        let sm = SkillManager::init(dir.path());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let list = rt.block_on(sm.list());
        assert!(list.contains("test-plugin"));
        assert!(list.contains("test-skill"));
    }

    #[test]
    fn registry_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry {
            skills: vec![RegistryEntry {
                name: "my-skill".to_string(),
                repo: "https://github.com/user/repo".to_string(),
                path: Some("plugins/my-skill".to_string()),
                installed_at: "2026-03-30T12:00:00Z".to_string(),
                version: Some("1.0.0".to_string()),
            }],
        };

        save_registry(dir.path(), &registry);
        let loaded = load_registry(dir.path());
        assert_eq!(loaded.skills.len(), 1);
        assert_eq!(loaded.skills[0].name, "my-skill");
        assert_eq!(loaded.skills[0].path.as_deref(), Some("plugins/my-skill"));
    }

    #[tokio::test]
    async fn remove_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let sm = SkillManager::init(dir.path());
        let result = sm.remove("nope").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not installed"));
    }

    #[tokio::test]
    async fn match_skills_returns_relevant() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();

        // Create plugin with skill
        let plugin_dir = skills_dir.join("web-plugin");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        setup_skill_dir(
            &plugin_dir,
            "web-research",
            "This skill should be used when the user asks to search the web or research topics online",
        );

        let registry = Registry {
            skills: vec![RegistryEntry {
                name: "web-plugin".to_string(),
                repo: "https://github.com/test/web".to_string(),
                path: None,
                installed_at: "2026-03-30T00:00:00Z".to_string(),
                version: None,
            }],
        };
        std::fs::write(
            skills_dir.join("registry.json"),
            serde_json::to_string_pretty(&registry).unwrap(),
        )
        .unwrap();

        let sm = SkillManager::init(dir.path());
        let matched = sm.match_skills("search the web for tutorials").await;
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].name, "web-research");
    }

    #[tokio::test]
    async fn match_skills_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();

        let plugin_dir = skills_dir.join("db-plugin");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        setup_skill_dir(&plugin_dir, "db-migrate", "database migration PostgreSQL schemas");

        let registry = Registry {
            skills: vec![RegistryEntry {
                name: "db-plugin".to_string(),
                repo: "https://github.com/test/db".to_string(),
                path: None,
                installed_at: "2026-03-30T00:00:00Z".to_string(),
                version: None,
            }],
        };
        std::fs::write(
            skills_dir.join("registry.json"),
            serde_json::to_string_pretty(&registry).unwrap(),
        )
        .unwrap();

        let sm = SkillManager::init(dir.path());
        let matched = sm.match_skills("tell me a joke").await;
        assert!(matched.is_empty());
    }
}
