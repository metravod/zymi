use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::Deserialize;

/// Metadata parsed from SKILL.md YAML frontmatter.
#[derive(Debug, Clone)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    pub version: Option<String>,
    pub body: String,
    /// Directory containing the SKILL.md — for resolving references/, scripts/.
    #[allow(dead_code)]
    pub base_dir: PathBuf,
}

#[derive(Debug, Deserialize)]
struct Frontmatter {
    name: String,
    description: String,
    #[serde(default)]
    version: Option<String>,
}

/// Parse a SKILL.md file: extract YAML frontmatter and markdown body.
pub fn parse_skill_md(path: &Path) -> Result<SkillMeta, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;

    let (frontmatter, body) = split_frontmatter(&content)
        .ok_or_else(|| format!("No YAML frontmatter found in {}", path.display()))?;

    let fm: Frontmatter = serde_yaml::from_str(frontmatter)
        .map_err(|e| format!("Invalid frontmatter in {}: {e}", path.display()))?;

    let base_dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();

    Ok(SkillMeta {
        name: fm.name,
        description: fm.description,
        version: fm.version,
        body: body.trim().to_string(),
        base_dir,
    })
}

/// Split `---\nfrontmatter\n---\nbody` into (frontmatter, body).
fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    // Skip the opening "---" line
    let after_open = &trimmed[3..];
    let after_open = after_open.strip_prefix('\n').unwrap_or(after_open);

    let close_pos = after_open.find("\n---")?;
    let frontmatter = &after_open[..close_pos];
    let body = &after_open[close_pos + 4..]; // skip "\n---"
    // Skip optional newline after closing ---
    let body = body.strip_prefix('\n').unwrap_or(body);

    Some((frontmatter, body))
}

/// Discover all SKILL.md files under a plugin directory.
///
/// Searches patterns:
/// - `skills/*/SKILL.md` (Claude Code plugin format)
/// - `SKILL.md` (standalone skill)
pub fn discover_skills(plugin_dir: &Path) -> Vec<SkillMeta> {
    let mut skills = Vec::new();

    // Pattern 1: skills/*/SKILL.md
    let skills_dir = plugin_dir.join("skills");
    if skills_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&skills_dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    let skill_md = entry.path().join("SKILL.md");
                    if skill_md.exists() {
                        match parse_skill_md(&skill_md) {
                            Ok(meta) => skills.push(meta),
                            Err(e) => log::warn!("Skipping {}: {e}", skill_md.display()),
                        }
                    }
                }
            }
        }
    }

    // Pattern 2: standalone SKILL.md at root
    if skills.is_empty() {
        let root_md = plugin_dir.join("SKILL.md");
        if root_md.exists() {
            match parse_skill_md(&root_md) {
                Ok(meta) => skills.push(meta),
                Err(e) => log::warn!("Skipping {}: {e}", root_md.display()),
            }
        }
    }

    skills
}

/// Check for `.mcp.json` in a plugin directory and return its parsed content.
pub fn discover_mcp(plugin_dir: &Path) -> Option<serde_json::Value> {
    let mcp_path = plugin_dir.join(".mcp.json");
    let content = std::fs::read_to_string(mcp_path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Clone a git repository into `target_dir`.
pub async fn git_clone(repo_url: &str, target_dir: &Path) -> Result<(), String> {
    let output = tokio::process::Command::new("git")
        .args(["clone", "--depth", "1", repo_url])
        .arg(target_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("Failed to run git clone: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git clone failed: {stderr}"));
    }

    Ok(())
}

/// Pull latest changes in an existing repo.
pub async fn git_pull(repo_dir: &Path) -> Result<(), String> {
    let output = tokio::process::Command::new("git")
        .args(["pull", "--ff-only"])
        .current_dir(repo_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("Failed to run git pull: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git pull failed: {stderr}"));
    }

    Ok(())
}

/// Derive a directory name from a git repo URL.
/// `https://github.com/user/repo.git` → `repo`
/// `https://github.com/user/repo` → `repo`
pub fn repo_dir_name(repo_url: &str) -> String {
    repo_url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("skill")
        .trim_end_matches(".git")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_skill_md() {
        let dir = tempfile::tempdir().unwrap();
        let skill_path = dir.path().join("SKILL.md");
        std::fs::write(
            &skill_path,
            "---\nname: test-skill\ndescription: A test skill for unit testing\nversion: 1.0.0\n---\n\n# Test Skill\n\nDo the thing.\n",
        )
        .unwrap();

        let meta = parse_skill_md(&skill_path).unwrap();
        assert_eq!(meta.name, "test-skill");
        assert_eq!(meta.description, "A test skill for unit testing");
        assert_eq!(meta.version.as_deref(), Some("1.0.0"));
        assert!(meta.body.contains("# Test Skill"));
        assert!(meta.body.contains("Do the thing."));
    }

    #[test]
    fn parse_skill_md_no_version() {
        let dir = tempfile::tempdir().unwrap();
        let skill_path = dir.path().join("SKILL.md");
        std::fs::write(
            &skill_path,
            "---\nname: minimal\ndescription: Minimal skill\n---\n\nBody here.\n",
        )
        .unwrap();

        let meta = parse_skill_md(&skill_path).unwrap();
        assert_eq!(meta.name, "minimal");
        assert!(meta.version.is_none());
        assert_eq!(meta.body, "Body here.");
    }

    #[test]
    fn parse_skill_md_no_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let skill_path = dir.path().join("SKILL.md");
        std::fs::write(&skill_path, "# Just markdown\n\nNo frontmatter here.\n").unwrap();

        let result = parse_skill_md(&skill_path);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No YAML frontmatter"));
    }

    #[test]
    fn parse_skill_md_invalid_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let skill_path = dir.path().join("SKILL.md");
        std::fs::write(
            &skill_path,
            "---\ninvalid: [yaml\n---\n\nBody.\n",
        )
        .unwrap();

        let result = parse_skill_md(&skill_path);
        assert!(result.is_err());
    }

    #[test]
    fn parse_skill_md_missing_name() {
        let dir = tempfile::tempdir().unwrap();
        let skill_path = dir.path().join("SKILL.md");
        std::fs::write(
            &skill_path,
            "---\ndescription: has desc but no name\n---\n\nBody.\n",
        )
        .unwrap();

        let result = parse_skill_md(&skill_path);
        assert!(result.is_err());
    }

    #[test]
    fn discover_skills_plugin_format() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("skills").join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: Does things\n---\n\nInstructions.\n",
        )
        .unwrap();

        let skills = discover_skills(dir.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "my-skill");
    }

    #[test]
    fn discover_skills_standalone() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("SKILL.md"),
            "---\nname: standalone\ndescription: Root skill\n---\n\nRoot body.\n",
        )
        .unwrap();

        let skills = discover_skills(dir.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "standalone");
    }

    #[test]
    fn discover_skills_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let skills = discover_skills(dir.path());
        assert!(skills.is_empty());
    }

    #[test]
    fn discover_mcp_present() {
        let dir = tempfile::tempdir().unwrap();
        let mcp = serde_json::json!({
            "test-server": {"command": "node", "args": ["server.js"]}
        });
        std::fs::write(
            dir.path().join(".mcp.json"),
            serde_json::to_string(&mcp).unwrap(),
        )
        .unwrap();

        let result = discover_mcp(dir.path());
        assert!(result.is_some());
    }

    #[test]
    fn discover_mcp_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(discover_mcp(dir.path()).is_none());
    }

    #[test]
    fn repo_dir_name_variants() {
        assert_eq!(repo_dir_name("https://github.com/user/repo.git"), "repo");
        assert_eq!(repo_dir_name("https://github.com/user/repo"), "repo");
        assert_eq!(repo_dir_name("https://github.com/user/repo/"), "repo");
        assert_eq!(repo_dir_name("git@github.com:user/my-skills.git"), "my-skills");
    }

    #[test]
    fn split_frontmatter_basic() {
        let content = "---\nname: test\n---\nBody text";
        let (fm, body) = split_frontmatter(content).unwrap();
        assert_eq!(fm, "name: test");
        assert_eq!(body, "Body text");
    }

    #[test]
    fn split_frontmatter_no_markers() {
        assert!(split_frontmatter("Just text").is_none());
    }
}
