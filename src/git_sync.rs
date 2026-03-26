use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

/// Handles git operations for the memory directory.
pub struct GitSync {
    dir: PathBuf,
    enabled: bool,
}

impl GitSync {
    /// Create a GitSync for an existing git repo in `dir`.
    /// If `dir` is not a git repository, all operations are no-ops.
    pub fn new(dir: &Path) -> Self {
        let enabled = dir.join(".git").is_dir();
        if enabled {
            log::info!("Git sync enabled for {}", dir.display());
        }
        Self {
            dir: dir.to_path_buf(),
            enabled,
        }
    }

    /// Initialize a new git repo in `dir` with a .gitignore.
    pub fn init_repo(dir: &Path) -> Result<(), String> {
        run_git(dir, &["init"]).map_err(|e| format!("git init failed: {e}"))?;

        let gitignore = dir.join(".gitignore");
        if !gitignore.exists() {
            let ignore = "conversations.db\nconversations.db-wal\nconversations.db-shm\n.heartbeat\nzymi.log\ntool_embeddings.json\nauth.json\n";
            std::fs::write(&gitignore, ignore)
                .map_err(|e| format!("Failed to write .gitignore: {e}"))?;
        }

        run_git(dir, &["add", "-A"]).ok();
        run_git(dir, &["commit", "-m", "initial commit", "--quiet"]).ok();

        Ok(())
    }

    /// Add a remote origin and push.
    pub fn add_remote(dir: &Path, url: &str) -> Result<(), String> {
        // Remove existing origin if present
        run_git(dir, &["remote", "remove", "origin"]).ok();
        run_git(dir, &["remote", "add", "origin", url])
            .map_err(|e| format!("Failed to add remote: {e}"))?;
        run_git(dir, &["push", "-u", "origin", "main", "--quiet"])
            .or_else(|_| {
                // Try 'master' if 'main' fails
                run_git(dir, &["push", "-u", "origin", "master", "--quiet"])
            })
            .map_err(|e| format!("Failed to push: {e}"))?;
        Ok(())
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Stage all changes and commit with the given message.
    /// Returns true if a commit was created.
    pub fn commit(&self, message: &str) -> bool {
        if !self.enabled {
            return false;
        }
        run_git(&self.dir, &["add", "-A"]).ok();
        let created = run_git(&self.dir, &["commit", "-m", message, "--quiet"])
            .map(|_| true)
            .unwrap_or(false);
        if created {
            log::info!("Git commit created: '{}'", message);
        }
        created
    }

    /// Push to remote if configured and there are unpushed commits.
    pub fn push(&self) -> bool {
        if !self.enabled || !self.has_remote() {
            return false;
        }
        log::info!("Git push started");
        let ok = run_git(&self.dir, &["push", "--quiet"])
            .map(|_| true)
            .unwrap_or(false);
        if !ok {
            log::warn!("Git push failed");
        }
        ok
    }

    /// Pull latest changes from remote.
    pub fn pull(&self) -> bool {
        if !self.enabled || !self.has_remote() {
            return false;
        }
        log::info!("Git pull started");
        let ok = run_git(&self.dir, &["pull", "--quiet", "--rebase"])
            .map(|_| true)
            .unwrap_or(false);
        if ok {
            log::info!("Git pull completed");
        } else {
            log::warn!("Git pull failed");
        }
        ok
    }

    fn has_remote(&self) -> bool {
        run_git(&self.dir, &["remote"])
            .map(|out| !out.trim().is_empty())
            .unwrap_or(false)
    }

    fn has_unpushed(&self) -> bool {
        if !self.has_remote() {
            return false;
        }
        // Update remote refs first (non-blocking check)
        run_git(&self.dir, &["fetch", "--quiet"]).ok();
        run_git(&self.dir, &["log", "@{u}..HEAD", "--oneline"])
            .map(|out| !out.trim().is_empty())
            .unwrap_or(false)
    }
}

fn run_git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let cmd_str = format!("git {}", args.join(" "));
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| format!("`{cmd_str}` failed to execute: {e}"))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "`{cmd_str}` exited with {}: {}",
            output.status,
            stderr.trim()
        ))
    }
}

/// Background heartbeat: writes a timestamp and pushes pending git commits.
/// Runs until `shutdown` is signalled via the CancellationToken.
pub async fn heartbeat(
    memory_dir: PathBuf,
    git_sync: Arc<GitSync>,
    shutdown: tokio_util::sync::CancellationToken,
    interval_secs: u64,
) {
    let heartbeat_path = memory_dir.join(".heartbeat");
    let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));

    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = shutdown.cancelled() => {
                log::info!("Heartbeat: shutdown signal received");
                break;
            }
        }

        // Write heartbeat timestamp
        let now = chrono::Utc::now().to_rfc3339();
        let _ = std::fs::write(&heartbeat_path, &now);

        if git_sync.is_enabled() {
            // Commit any uncommitted changes (e.g. summaries, config edits)
            git_sync.commit("auto-commit");

            // Push if there are unpushed commits
            if git_sync.has_unpushed() {
                if git_sync.push() {
                    log::info!("Heartbeat: git push ok");
                } else {
                    log::warn!("Heartbeat: git push failed");
                }
            }
        }
    }
}
