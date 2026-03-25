pub mod bubblewrap;
pub mod native;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Execution context determines which sandbox profile to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionContext {
    /// User-initiated shell command (main agent, interactive)
    Interactive,
    /// Workflow CreateTool / CodeGen / InstallDep nodes
    Workflow,
    /// Scheduler-triggered autonomous execution
    Scheduler,
    /// Sub-agent or background task
    SubAgent,
    /// RunCodeTool (temp file + interpreter)
    RunCode,
}

/// A filesystem path to expose inside the sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsRule {
    pub path: String,
    #[serde(default)]
    pub writable: bool,
}

/// Per-context sandbox constraints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxProfile {
    /// Filesystem paths to allow. Everything else is denied.
    #[serde(default)]
    pub fs_allow: Vec<FsRule>,
    /// Paths explicitly denied even if a parent is allowed.
    #[serde(default)]
    pub fs_deny: Vec<String>,
    /// Allow network access.
    #[serde(default = "default_true")]
    pub network: bool,
    /// Max memory in bytes (0 = unlimited).
    #[serde(default)]
    pub memory_limit_bytes: u64,
    /// Max PIDs (0 = unlimited).
    #[serde(default)]
    pub max_pids: u32,
    /// If true, sandboxed commands skip approval even if ShellTool would require it.
    #[serde(default)]
    pub auto_approve: bool,
}

impl Default for SandboxProfile {
    fn default() -> Self {
        Self {
            fs_allow: vec![],
            fs_deny: vec![],
            network: true,
            memory_limit_bytes: 0,
            max_pids: 0,
            auto_approve: false,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxBackendType {
    #[default]
    Native,
    Bubblewrap,
}

/// Top-level sandbox configuration, loaded from `sandbox.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub default_backend: SandboxBackendType,
    #[serde(default)]
    pub profiles: HashMap<ExecutionContext, SandboxProfile>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            default_backend: SandboxBackendType::Native,
            profiles: HashMap::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// SandboxedCommand — the result of wrapping a command
// ---------------------------------------------------------------------------

/// A command ready for execution, possibly wrapped by a sandbox backend.
pub struct SandboxedCommand {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

// ---------------------------------------------------------------------------
// SandboxManager
// ---------------------------------------------------------------------------

pub struct SandboxManager {
    config: SandboxConfig,
    available_backends: HashSet<SandboxBackendType>,
}

impl SandboxManager {
    /// Create a new sandbox manager. Detects available backends.
    pub async fn new(config: SandboxConfig) -> Self {
        let mut available = HashSet::new();
        available.insert(SandboxBackendType::Native);

        // Detect bubblewrap
        if cfg!(target_os = "linux") {
            match tokio::process::Command::new("which")
                .arg("bwrap")
                .output()
                .await
            {
                Ok(output) if output.status.success() => {
                    available.insert(SandboxBackendType::Bubblewrap);
                    log::info!("Sandbox: bubblewrap backend available");
                }
                _ => {
                    log::debug!("Sandbox: bubblewrap not found");
                }
            }
        }

        if config.enabled {
            let backend = config.default_backend;
            if !available.contains(&backend) {
                log::warn!(
                    "Sandbox enabled but default backend {backend:?} is not available. \
                     Commands will fall back to native (unsandboxed)."
                );
            } else {
                log::info!("Sandbox enabled with backend: {backend:?}");
            }
        }

        Self {
            config,
            available_backends: available,
        }
    }

    #[allow(dead_code)]
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Returns true if the sandbox is enabled AND a real (non-native) backend is available.
    pub fn is_active(&self) -> bool {
        self.config.enabled
            && self.config.default_backend != SandboxBackendType::Native
            && self.available_backends.contains(&self.config.default_backend)
    }

    pub fn profile_for(&self, ctx: ExecutionContext) -> &SandboxProfile {
        static DEFAULT_PROFILE: SandboxProfile = SandboxProfile {
            fs_allow: vec![],
            fs_deny: vec![],
            network: true,
            memory_limit_bytes: 0,
            max_pids: 0,
            auto_approve: false,
        };
        self.config.profiles.get(&ctx).unwrap_or(&DEFAULT_PROFILE)
    }

    /// Wrap a shell command (`sh -c <command>`) for sandboxed execution.
    pub fn wrap_command(
        &self,
        ctx: ExecutionContext,
        command: &str,
        working_dir: Option<&Path>,
    ) -> SandboxedCommand {
        if !self.config.enabled {
            return native::wrap_command(command);
        }

        let profile = self.profile_for(ctx);
        let backend = self.config.default_backend;

        if !self.available_backends.contains(&backend) {
            log::warn!(
                "Sandbox backend {backend:?} not available, falling back to native (no sandbox)"
            );
            return native::wrap_command(command);
        }

        match backend {
            SandboxBackendType::Native => native::wrap_command(command),
            SandboxBackendType::Bubblewrap => {
                bubblewrap::wrap_command(profile, command, working_dir)
            }
        }
    }

    /// Wrap an interpreter + script execution for sandboxed execution.
    pub fn wrap_script(
        &self,
        ctx: ExecutionContext,
        interpreter: &str,
        script_path: &Path,
        working_dir: Option<&Path>,
    ) -> SandboxedCommand {
        if !self.config.enabled {
            return native::wrap_script(interpreter, script_path);
        }

        let profile = self.profile_for(ctx);
        let backend = self.config.default_backend;

        if !self.available_backends.contains(&backend) {
            log::warn!(
                "Sandbox backend {backend:?} not available, falling back to native (no sandbox)"
            );
            return native::wrap_script(interpreter, script_path);
        }

        match backend {
            SandboxBackendType::Native => native::wrap_script(interpreter, script_path),
            SandboxBackendType::Bubblewrap => {
                bubblewrap::wrap_script(profile, interpreter, script_path, working_dir)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Config loading
// ---------------------------------------------------------------------------

/// Load sandbox configuration from `{memory_dir}/sandbox.json`.
/// Returns disabled config if the file does not exist.
pub fn load_sandbox_config(memory_dir: &Path) -> SandboxConfig {
    let path = memory_dir.join("sandbox.json");
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let mut config: SandboxConfig =
                serde_json::from_str(&content).unwrap_or_else(|e| {
                    log::warn!("Failed to parse sandbox.json: {e}, sandbox disabled");
                    SandboxConfig::default()
                });
            expand_templates(&mut config, memory_dir);
            config
        }
        Err(_) => SandboxConfig::default(),
    }
}

/// Expand `{{memory_dir}}` and `{{home}}` template variables in profile paths.
fn expand_templates(config: &mut SandboxConfig, memory_dir: &Path) {
    let memory_dir_str = memory_dir.to_string_lossy().to_string();
    let home = std::env::var("HOME").unwrap_or_default();

    for profile in config.profiles.values_mut() {
        for rule in &mut profile.fs_allow {
            rule.path = rule
                .path
                .replace("{{memory_dir}}", &memory_dir_str)
                .replace("{{home}}", &home);
        }
        for deny_path in &mut profile.fs_deny {
            *deny_path = deny_path
                .replace("{{memory_dir}}", &memory_dir_str)
                .replace("{{home}}", &home);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_disabled() {
        let config = SandboxConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.default_backend, SandboxBackendType::Native);
        assert!(config.profiles.is_empty());
    }

    #[test]
    fn default_profile_values() {
        let manager_config = SandboxConfig::default();
        let mgr = SandboxManager {
            config: manager_config,
            available_backends: HashSet::new(),
        };
        let profile = mgr.profile_for(ExecutionContext::Interactive);
        assert!(profile.network);
        assert_eq!(profile.memory_limit_bytes, 0);
        assert_eq!(profile.max_pids, 0);
        assert!(!profile.auto_approve);
    }

    #[test]
    fn expand_templates_replaces_vars() {
        let mut config = SandboxConfig {
            enabled: true,
            default_backend: SandboxBackendType::Bubblewrap,
            profiles: HashMap::from([(
                ExecutionContext::Workflow,
                SandboxProfile {
                    fs_allow: vec![
                        FsRule {
                            path: "{{memory_dir}}/scripts".into(),
                            writable: true,
                        },
                        FsRule {
                            path: "{{home}}/.config".into(),
                            writable: false,
                        },
                    ],
                    fs_deny: vec!["{{home}}/.ssh".into()],
                    ..Default::default()
                },
            )]),
        };

        let memory_dir = Path::new("/data/zymi");
        expand_templates(&mut config, memory_dir);

        let profile = &config.profiles[&ExecutionContext::Workflow];
        assert_eq!(profile.fs_allow[0].path, "/data/zymi/scripts");
        assert!(profile.fs_allow[1].path.ends_with("/.config"));
        assert!(profile.fs_deny[0].ends_with("/.ssh"));
    }

    #[test]
    fn deserialize_config_from_json() {
        let json = r#"{
            "enabled": true,
            "default_backend": "bubblewrap",
            "profiles": {
                "workflow": {
                    "fs_allow": [{"path": "/tmp", "writable": true}],
                    "fs_deny": ["/etc/shadow"],
                    "network": false,
                    "memory_limit_bytes": 268435456,
                    "max_pids": 50,
                    "auto_approve": true
                }
            }
        }"#;

        let config: SandboxConfig = serde_json::from_str(json).unwrap();
        assert!(config.enabled);
        assert_eq!(config.default_backend, SandboxBackendType::Bubblewrap);

        let profile = &config.profiles[&ExecutionContext::Workflow];
        assert_eq!(profile.fs_allow.len(), 1);
        assert!(profile.fs_allow[0].writable);
        assert!(!profile.network);
        assert_eq!(profile.memory_limit_bytes, 268_435_456);
        assert_eq!(profile.max_pids, 50);
        assert!(profile.auto_approve);
    }

    #[test]
    fn load_sandbox_config_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = load_sandbox_config(dir.path());
        assert!(!config.enabled);
    }

    #[test]
    fn load_sandbox_config_valid_file() {
        let dir = tempfile::tempdir().unwrap();
        let json = r#"{"enabled": true, "default_backend": "bubblewrap"}"#;
        std::fs::write(dir.path().join("sandbox.json"), json).unwrap();

        let config = load_sandbox_config(dir.path());
        assert!(config.enabled);
        assert_eq!(config.default_backend, SandboxBackendType::Bubblewrap);
    }

    #[test]
    fn load_sandbox_config_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sandbox.json"), "not json!").unwrap();

        let config = load_sandbox_config(dir.path());
        assert!(!config.enabled); // falls back to default
    }

    #[tokio::test]
    async fn manager_disabled_uses_native() {
        let config = SandboxConfig::default();
        let mgr = SandboxManager::new(config).await;
        assert!(!mgr.is_enabled());
        assert!(!mgr.is_active());

        let cmd = mgr.wrap_command(ExecutionContext::Interactive, "echo hello", None);
        assert_eq!(cmd.program, "sh");
        assert_eq!(cmd.args, vec!["-c", "echo hello"]);
    }

    #[tokio::test]
    async fn manager_is_active_requires_real_backend() {
        let config = SandboxConfig {
            enabled: true,
            default_backend: SandboxBackendType::Native,
            profiles: HashMap::new(),
        };
        let mgr = SandboxManager::new(config).await;
        assert!(mgr.is_enabled());
        assert!(!mgr.is_active()); // Native is not a "real" backend
    }
}
