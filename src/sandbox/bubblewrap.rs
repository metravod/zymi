use std::path::Path;

use super::{FsRule, SandboxProfile, SandboxedCommand};

/// System paths to bind read-only inside the sandbox.
const SYSTEM_RO_BINDS: &[&str] = &["/usr", "/bin", "/sbin", "/lib", "/lib64", "/etc"];

/// Build bwrap arguments from a sandbox profile for `sh -c <command>`.
pub fn wrap_command(
    profile: &SandboxProfile,
    command: &str,
    working_dir: Option<&Path>,
) -> SandboxedCommand {
    let inner = build_resource_prefix(profile, command);
    let args = build_bwrap_args(profile, working_dir, &["sh", "-c", &inner]);
    SandboxedCommand {
        program: "bwrap".into(),
        args,
        env: vec![],
    }
}

/// Build bwrap arguments from a sandbox profile for `<interpreter> <script_path>`.
pub fn wrap_script(
    profile: &SandboxProfile,
    interpreter: &str,
    script_path: &Path,
    working_dir: Option<&Path>,
) -> SandboxedCommand {
    let script_str = script_path.to_string_lossy();

    // The script file must be readable inside the sandbox.
    // We bind its parent directory read-only so bwrap can access it.
    let extra_ro: Vec<FsRule> = script_path
        .parent()
        .map(|p| {
            vec![FsRule {
                path: p.to_string_lossy().into(),
                writable: false,
            }]
        })
        .unwrap_or_default();

    let inner_cmd = build_resource_prefix(profile, &format!("{interpreter} {script_str}"));
    let mut effective_profile = profile.clone();
    effective_profile.fs_allow.extend(extra_ro);

    let args = build_bwrap_args(
        &effective_profile,
        working_dir,
        &["sh", "-c", &inner_cmd],
    );
    SandboxedCommand {
        program: "bwrap".into(),
        args,
        env: vec![],
    }
}

/// Build the bwrap argument list.
fn build_bwrap_args(
    profile: &SandboxProfile,
    working_dir: Option<&Path>,
    inner_argv: &[&str],
) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();

    // System read-only binds
    for path in SYSTEM_RO_BINDS {
        if Path::new(path).exists() {
            args.extend(["--ro-bind".into(), (*path).into(), (*path).into()]);
        }
    }

    // /proc and /dev
    args.extend(["--proc".into(), "/proc".into()]);
    args.extend(["--dev".into(), "/dev".into()]);

    // /tmp as tmpfs (unless user explicitly allows it writable)
    let tmp_explicitly_allowed = profile
        .fs_allow
        .iter()
        .any(|r| r.path == "/tmp" && r.writable);
    if !tmp_explicitly_allowed {
        args.extend(["--tmpfs".into(), "/tmp".into()]);
    }

    // User-defined filesystem rules
    for rule in &profile.fs_allow {
        if !Path::new(&rule.path).exists() {
            continue;
        }
        if rule.writable {
            args.extend(["--bind".into(), rule.path.clone(), rule.path.clone()]);
        } else {
            args.extend(["--ro-bind".into(), rule.path.clone(), rule.path.clone()]);
        }
    }

    // Network isolation
    if !profile.network {
        args.push("--unshare-net".into());
    }

    // Safety flags
    args.push("--die-with-parent".into());
    args.push("--new-session".into());

    // Working directory
    if let Some(wd) = working_dir {
        let wd_str = wd.to_string_lossy().to_string();
        // Ensure working dir is accessible (bind if not already allowed)
        let already_bound = profile.fs_allow.iter().any(|r| r.path == wd_str);
        if !already_bound && Path::new(&wd_str).exists() {
            args.extend(["--bind".into(), wd_str.clone(), wd_str.clone()]);
        }
        args.extend(["--chdir".into(), wd_str]);
    }

    // Separator
    args.push("--".into());

    // Inner command
    args.extend(inner_argv.iter().map(|s| s.to_string()));

    args
}

/// Prefix the command with ulimit calls if resource limits are set.
fn build_resource_prefix(profile: &SandboxProfile, command: &str) -> String {
    let mut parts = Vec::new();

    if profile.memory_limit_bytes > 0 {
        // ulimit -v takes kilobytes
        let kb = profile.memory_limit_bytes / 1024;
        parts.push(format!("ulimit -v {kb}"));
    }

    if profile.max_pids > 0 {
        parts.push(format!("ulimit -u {}", profile.max_pids));
    }

    if parts.is_empty() {
        command.to_string()
    } else {
        parts.push(command.to_string());
        parts.join(" && ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_profile() -> SandboxProfile {
        SandboxProfile {
            fs_allow: vec![
                FsRule {
                    path: "/tmp".into(),
                    writable: true,
                },
            ],
            fs_deny: vec![],
            network: true,
            memory_limit_bytes: 0,
            max_pids: 0,
            auto_approve: false,
        }
    }

    #[test]
    fn wrap_command_basic() {
        let profile = test_profile();
        let cmd = wrap_command(&profile, "echo hello", None);
        assert_eq!(cmd.program, "bwrap");
        assert!(cmd.args.contains(&"--die-with-parent".to_string()));
        assert!(cmd.args.contains(&"--new-session".to_string()));
        assert!(cmd.args.contains(&"--".to_string()));

        // Inner command is last 3 args: sh -c "echo hello"
        let len = cmd.args.len();
        assert_eq!(cmd.args[len - 3], "sh");
        assert_eq!(cmd.args[len - 2], "-c");
        assert_eq!(cmd.args[len - 1], "echo hello");
    }

    #[test]
    fn wrap_command_network_isolation() {
        let mut profile = test_profile();
        profile.network = false;
        let cmd = wrap_command(&profile, "curl example.com", None);
        assert!(cmd.args.contains(&"--unshare-net".to_string()));
    }

    #[test]
    fn wrap_command_network_allowed() {
        let profile = test_profile();
        let cmd = wrap_command(&profile, "curl example.com", None);
        assert!(!cmd.args.contains(&"--unshare-net".to_string()));
    }

    #[test]
    fn wrap_command_with_resource_limits() {
        let mut profile = test_profile();
        profile.memory_limit_bytes = 268_435_456; // 256 MB
        profile.max_pids = 50;

        let cmd = wrap_command(&profile, "echo hello", None);
        let inner = &cmd.args[cmd.args.len() - 1];
        assert!(inner.contains("ulimit -v 262144"));
        assert!(inner.contains("ulimit -u 50"));
        assert!(inner.contains("echo hello"));
    }

    #[test]
    fn wrap_command_writable_bind() {
        let profile = test_profile();
        let cmd = wrap_command(&profile, "touch /tmp/test", None);
        // /tmp should be --bind (writable), not --ro-bind
        let bind_idx = cmd
            .args
            .windows(3)
            .position(|w| w[0] == "--bind" && w[1] == "/tmp" && w[2] == "/tmp");
        assert!(bind_idx.is_some(), "expected --bind /tmp /tmp in args");
    }

    #[test]
    fn wrap_script_basic() {
        let profile = test_profile();
        let cmd = wrap_script(&profile, "python3", Path::new("/tmp/test.py"), None);
        assert_eq!(cmd.program, "bwrap");
        let inner = &cmd.args[cmd.args.len() - 1];
        assert!(inner.contains("python3 /tmp/test.py"));
    }

    #[test]
    fn build_resource_prefix_empty() {
        let profile = test_profile();
        let result = build_resource_prefix(&profile, "echo hi");
        assert_eq!(result, "echo hi");
    }

    #[test]
    fn build_resource_prefix_with_limits() {
        let profile = SandboxProfile {
            memory_limit_bytes: 1_048_576, // 1 MB
            max_pids: 10,
            ..Default::default()
        };
        let result = build_resource_prefix(&profile, "echo hi");
        assert_eq!(result, "ulimit -v 1024 && ulimit -u 10 && echo hi");
    }

    #[test]
    fn system_binds_included() {
        let profile = test_profile();
        let cmd = wrap_command(&profile, "ls", None);
        // At least /usr should be bound (it always exists)
        let has_usr = cmd
            .args
            .windows(3)
            .any(|w| w[0] == "--ro-bind" && w[1] == "/usr" && w[2] == "/usr");
        assert!(has_usr, "expected --ro-bind /usr /usr");
    }

    #[test]
    fn working_dir_creates_bind_and_chdir() {
        let profile = test_profile();
        let cmd = wrap_command(&profile, "ls", Some(Path::new("/tmp")));
        // /tmp is already in fs_allow, so no extra bind, but --chdir should be present
        assert!(cmd.args.contains(&"--chdir".to_string()));
        assert!(cmd.args.contains(&"/tmp".to_string()));
    }
}
