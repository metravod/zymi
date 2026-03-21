use std::path::Path;

use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[derive(Default)]
pub struct PolicyConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Commands always allowed (glob patterns). Matched first.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Commands always denied (glob patterns). Matched second.
    #[serde(default)]
    pub deny: Vec<String>,
    /// Commands that require human approval even if allow matches (glob patterns).
    #[serde(default)]
    pub require_approval: Vec<String>,
}


#[derive(Debug, Clone, PartialEq)]
pub enum PolicyDecision {
    /// Command is allowed to execute
    Allow,
    /// Command requires human approval before execution
    RequireApproval,
    /// Command is denied
    Deny(String),
}

pub struct PolicyEngine {
    config: PolicyConfig,
    allow_patterns: Vec<Regex>,
    deny_patterns: Vec<Regex>,
    approval_patterns: Vec<Regex>,
}

impl PolicyEngine {
    pub fn new(config: PolicyConfig) -> Self {
        let allow_patterns = config
            .allow
            .iter()
            .filter_map(|p| glob_to_regex(p))
            .collect();
        let deny_patterns = config
            .deny
            .iter()
            .filter_map(|p| glob_to_regex(p))
            .collect();
        let approval_patterns = config
            .require_approval
            .iter()
            .filter_map(|p| glob_to_regex(p))
            .collect();

        Self {
            config,
            allow_patterns,
            deny_patterns,
            approval_patterns,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Evaluate a command against the policy rules.
    /// Order: deny > require_approval > allow > default (require_approval)
    pub fn evaluate(&self, command: &str) -> PolicyDecision {
        if !self.config.enabled {
            return PolicyDecision::RequireApproval;
        }

        // Extract the base command (first word or everything before the pipe/semicolon)
        let normalized = command.trim();

        // Check deny list first (highest priority)
        for (i, re) in self.deny_patterns.iter().enumerate() {
            if re.is_match(normalized) {
                return PolicyDecision::Deny(format!(
                    "Command matches deny rule: {}",
                    self.config.deny[i]
                ));
            }
        }

        // Check dangerous patterns (always denied regardless of config)
        if let Some(reason) = check_dangerous(normalized) {
            return PolicyDecision::Deny(reason);
        }

        // Check require_approval list
        for re in &self.approval_patterns {
            if re.is_match(normalized) {
                return PolicyDecision::RequireApproval;
            }
        }

        // Check allow list
        for re in &self.allow_patterns {
            if re.is_match(normalized) {
                return PolicyDecision::Allow;
            }
        }

        // Default: require approval (safe default)
        PolicyDecision::RequireApproval
    }
}

/// Check for inherently dangerous commands that should always be denied
/// regardless of policy configuration.
fn check_dangerous(command: &str) -> Option<String> {
    let lower = command.to_lowercase();

    let dangerous = [
        ("rm -rf /", "Recursive deletion of root filesystem"),
        ("rm -rf /*", "Recursive deletion of root filesystem"),
        ("mkfs.", "Filesystem formatting"),
        (":(){:|:&};:", "Fork bomb"),
        ("dd if=/dev/zero of=/dev/sd", "Disk overwrite"),
        ("dd if=/dev/random of=/dev/sd", "Disk overwrite"),
        ("> /dev/sda", "Disk overwrite"),
        ("chmod -r 777 /", "Recursive permission change on root"),
        ("chown -r", "Recursive ownership change"),
    ];

    for (pattern, reason) in &dangerous {
        if lower.contains(pattern) {
            return Some(format!("Blocked: {reason}"));
        }
    }

    // Docker privilege escalation — container escape to host root
    if check_docker_escape(&lower) {
        return Some("Blocked: Docker privilege escalation (container escape to host root)".into());
    }

    // nsenter into PID 1 — direct host namespace escape (not Docker-specific)
    if lower.contains("nsenter") && lower.contains("-t 1") {
        return Some("Blocked: nsenter into PID 1 (host namespace escape)".into());
    }

    None
}

/// Detect Docker commands that grant root-equivalent access to the host.
///
/// The docker group gives broad API access (needed for container management).
/// These checks block specific escalation vectors that turn Docker access
/// into unrestricted host root access.
///
/// **Limitation:** this operates on the raw command string before `sh -c`
/// interprets it. Shell variable expansion, subshells, and complex quoting
/// can theoretically bypass these checks. Defense-in-depth: the default
/// policy is `require_approval`, so a human sees the command in Telegram
/// before execution. These hardcoded checks catch what LLMs typically generate.
fn check_docker_escape(lower: &str) -> bool {
    if !lower.contains("docker") {
        return false;
    }

    // --privileged: full host device access + all capabilities
    if lower.contains("--privileged") {
        return true;
    }

    // --device: raw host device access (e.g., /dev/sda1)
    if lower.contains("--device") {
        return true;
    }

    // --pid=host: host PID namespace (can ptrace any process)
    if lower.contains("--pid=host") || lower.contains("--pid host") {
        return true;
    }

    // --cap-add=all or --cap-add=sys_admin: near-root capabilities
    if lower.contains("--cap-add=all") || lower.contains("--cap-add all") {
        return true;
    }
    if lower.contains("--cap-add=sys_admin") || lower.contains("--cap-add sys_admin") {
        return true;
    }

    // --security-opt apparmor:unconfined — disables AppArmor
    if lower.contains("apparmor:unconfined") || lower.contains("apparmor=unconfined") {
        return true;
    }

    // Bind mounts from sensitive host paths
    if has_sensitive_mount(lower) {
        return true;
    }

    false
}

/// Host paths that must never be bind-mounted into a container.
/// Mounting these gives the container write access to critical host config,
/// credentials, or the Docker socket itself (DinD escape).
const SENSITIVE_MOUNT_SOURCES: &[&str] = &[
    "/etc",                    // shadow, sudoers, passwd, fstab, ssh configs
    "/root",                   // root SSH keys, shell history
    "/var/run/docker.sock",    // Docker-in-Docker escape
    "/run/docker.sock",        // alternative socket path
    "/proc",                   // host process info
    "/sys",                    // kernel interfaces
    "/dev",                    // host devices
    "/boot",                   // bootloader, initramfs
];

/// Extract bind-mount source paths from -v/--volume/--mount flags,
/// normalize them (strip quotes), and check against the sensitive blocklist.
fn has_sensitive_mount(lower: &str) -> bool {
    let sources = extract_mount_sources(lower);
    for raw in &sources {
        let path = normalize_path(raw);
        if is_sensitive_path(&path) {
            return true;
        }
    }
    false
}

/// Strip surrounding quotes and whitespace from a path extracted from a command.
fn normalize_path(raw: &str) -> String {
    raw.trim()
        .trim_matches(|c: char| c == '"' || c == '\'')
        .trim()
        .to_string()
}

/// Check if a path is exactly root `/` or falls under a sensitive prefix.
fn is_sensitive_path(path: &str) -> bool {
    // Exact root mount
    if path == "/" {
        return true;
    }

    for sensitive in SENSITIVE_MOUNT_SOURCES {
        // Exact match: -v /etc:/...
        if path == *sensitive {
            return true;
        }
        // Subpath: -v /etc/shadow:/...
        if path.starts_with(sensitive) && path.as_bytes().get(sensitive.len()) == Some(&b'/') {
            return true;
        }
    }

    false
}

/// Extract host source paths from docker volume/mount flags.
///
/// Handles formats:
/// - `-v HOST:CONTAINER`, `-v=HOST:CONTAINER`
/// - `--volume HOST:CONTAINER`, `--volume=HOST:CONTAINER`
/// - `--mount type=bind,source=HOST,target=CONTAINER`
/// - `--mount type=bind,src=HOST,target=CONTAINER`
fn extract_mount_sources(cmd: &str) -> Vec<String> {
    let mut sources = Vec::new();

    // -v / --volume: extract HOST from HOST:CONTAINER
    for flag in ["-v ", "-v=", "--volume ", "--volume="] {
        let mut search_from = 0;
        while let Some(offset) = cmd[search_from..].find(flag) {
            let arg_start = search_from + offset + flag.len();
            if arg_start >= cmd.len() {
                break;
            }
            // Take the next whitespace-delimited token
            let rest = cmd[arg_start..].trim_start();
            let token = rest.split_whitespace().next().unwrap_or("");
            // HOST:CONTAINER — split on first colon not preceded by windows drive letter
            if let Some(colon_pos) = token.find(':') {
                let host_part = &token[..colon_pos];
                if !host_part.is_empty() {
                    sources.push(host_part.to_string());
                }
            }
            search_from = arg_start + 1;
        }
    }

    // --mount: extract source= or src= value
    for flag in ["--mount ", "--mount="] {
        let mut search_from = 0;
        while let Some(offset) = cmd[search_from..].find(flag) {
            let arg_start = search_from + offset + flag.len();
            if arg_start >= cmd.len() {
                break;
            }
            let rest = cmd[arg_start..].trim_start();
            let token = rest.split_whitespace().next().unwrap_or("");
            // Parse comma-separated key=value pairs
            for part in token.split(',') {
                for key in ["source=", "src="] {
                    if let Some(stripped) = part.strip_prefix(key) {
                        if !stripped.is_empty() {
                            sources.push(stripped.to_string());
                        }
                    }
                }
            }
            search_from = arg_start + 1;
        }
    }

    sources
}

/// Convert a simple glob pattern to a regex.
/// Supports `*` (any characters) and `?` (single character).
fn glob_to_regex(pattern: &str) -> Option<Regex> {
    let mut regex_str = String::from("(?i)");
    for ch in pattern.chars() {
        match ch {
            '*' => regex_str.push_str(".*"),
            '?' => regex_str.push('.'),
            '.' | '+' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' | '\\' => {
                regex_str.push('\\');
                regex_str.push(ch);
            }
            _ => regex_str.push(ch),
        }
    }

    match Regex::new(&regex_str) {
        Ok(re) => Some(re),
        Err(e) => {
            log::warn!("Invalid policy pattern '{}': {e}", pattern);
            None
        }
    }
}

pub fn load_policy(memory_dir: &Path) -> PolicyConfig {
    let path = memory_dir.join("policy.json");
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_else(|e| {
            log::warn!("Failed to parse policy.json: {e}, policy disabled");
            PolicyConfig::default()
        }),
        Err(_) => PolicyConfig::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(allow: &[&str], deny: &[&str], approval: &[&str]) -> PolicyEngine {
        PolicyEngine::new(PolicyConfig {
            enabled: true,
            allow: allow.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
            require_approval: approval.iter().map(|s| s.to_string()).collect(),
        })
    }

    #[test]
    fn allow_matches() {
        let e = engine(&["echo *", "ls *", "cat *"], &[], &[]);
        assert_eq!(e.evaluate("echo hello"), PolicyDecision::Allow);
        assert_eq!(e.evaluate("ls -la /tmp"), PolicyDecision::Allow);
    }

    #[test]
    fn deny_overrides_allow() {
        let e = engine(&["rm *"], &["rm -rf *"], &[]);
        assert_eq!(
            e.evaluate("rm file.txt"),
            PolicyDecision::Allow
        );
        match e.evaluate("rm -rf /tmp/foo") {
            PolicyDecision::Deny(_) => {}
            other => panic!("Expected Deny, got {:?}", other),
        }
    }

    #[test]
    fn require_approval_overrides_allow() {
        let e = engine(
            &["systemctl *"],
            &[],
            &["systemctl restart *", "systemctl stop *"],
        );
        assert_eq!(e.evaluate("systemctl status nginx"), PolicyDecision::Allow);
        assert_eq!(
            e.evaluate("systemctl restart nginx"),
            PolicyDecision::RequireApproval
        );
    }

    #[test]
    fn default_is_require_approval() {
        let e = engine(&["echo *"], &[], &[]);
        assert_eq!(e.evaluate("unknown-command"), PolicyDecision::RequireApproval);
    }

    #[test]
    fn disabled_engine_always_requires_approval() {
        let e = PolicyEngine::new(PolicyConfig::default());
        assert_eq!(e.evaluate("echo hello"), PolicyDecision::RequireApproval);
    }

    #[test]
    fn dangerous_commands_always_denied() {
        let e = engine(&["*"], &[], &[]); // Allow everything
        match e.evaluate("rm -rf /") {
            PolicyDecision::Deny(_) => {}
            other => panic!("Expected Deny for rm -rf /, got {:?}", other),
        }
        match e.evaluate("dd if=/dev/zero of=/dev/sda") {
            PolicyDecision::Deny(_) => {}
            other => panic!("Expected Deny for dd, got {:?}", other),
        }
    }

    #[test]
    fn glob_patterns() {
        let e = engine(&["docker ps *", "docker logs *"], &["docker rm *"], &[]);
        assert_eq!(e.evaluate("docker ps -a"), PolicyDecision::Allow);
        match e.evaluate("docker rm my-container") {
            PolicyDecision::Deny(_) => {}
            other => panic!("Expected Deny, got {:?}", other),
        }
    }

    // ── Docker: privilege escalation flags ─────────────────────────

    #[test]
    fn docker_privileged_denied() {
        let e = engine(&["*"], &[], &[]);
        for cmd in [
            "docker run --privileged ubuntu",
            "docker run -it --privileged nginx sh",
            "docker create --privileged alpine",
        ] {
            match e.evaluate(cmd) {
                PolicyDecision::Deny(msg) => assert!(msg.contains("Docker"), "cmd: {cmd}"),
                other => panic!("Expected Deny for '{cmd}', got {:?}", other),
            }
        }
    }

    #[test]
    fn docker_device_denied() {
        let e = engine(&["*"], &[], &[]);
        for cmd in [
            "docker run --device /dev/sda1 ubuntu",
            "docker run --device=/dev/mem ubuntu",
            "docker run --device /dev/kvm ubuntu",
        ] {
            match e.evaluate(cmd) {
                PolicyDecision::Deny(_) => {}
                other => panic!("Expected Deny for '{cmd}', got {:?}", other),
            }
        }
    }

    #[test]
    fn docker_namespace_escape_denied() {
        let e = engine(&["*"], &[], &[]);
        for cmd in [
            "docker run --pid=host ubuntu",
            "docker run --cap-add=ALL ubuntu",
            "docker run --cap-add=SYS_ADMIN ubuntu",
            "docker run --security-opt apparmor:unconfined ubuntu",
            "nsenter -t 1 -m -u -i -n -p sh",
        ] {
            match e.evaluate(cmd) {
                PolicyDecision::Deny(_) => {}
                other => panic!("Expected Deny for '{cmd}', got {:?}", other),
            }
        }
    }

    // ── Docker: sensitive bind mounts ───────────────────────────────

    #[test]
    fn docker_root_mount_denied() {
        let e = engine(&["*"], &[], &[]);
        for cmd in [
            "docker run -v /:/host ubuntu",
            "docker run --volume=/:/mnt alpine sh",
            "docker run --mount type=bind,source=/,target=/host ubuntu",
            "docker run --mount src=/,target=/mnt alpine",
        ] {
            match e.evaluate(cmd) {
                PolicyDecision::Deny(_) => {}
                other => panic!("Expected Deny for '{cmd}', got {:?}", other),
            }
        }
    }

    #[test]
    fn docker_etc_mount_denied() {
        let e = engine(&["*"], &[], &[]);
        for cmd in [
            "docker run -v /etc:/host_etc ubuntu",
            "docker run -v /etc/shadow:/shadow ubuntu",
            "docker run --volume /etc/sudoers.d:/sudoers ubuntu",
            "docker run --mount type=bind,source=/etc,target=/cfg ubuntu",
        ] {
            match e.evaluate(cmd) {
                PolicyDecision::Deny(_) => {}
                other => panic!("Expected Deny for '{cmd}', got {:?}", other),
            }
        }
    }

    #[test]
    fn docker_root_home_mount_denied() {
        let e = engine(&["*"], &[], &[]);
        for cmd in [
            "docker run -v /root:/host_root ubuntu",
            "docker run -v /root/.ssh:/ssh ubuntu",
        ] {
            match e.evaluate(cmd) {
                PolicyDecision::Deny(_) => {}
                other => panic!("Expected Deny for '{cmd}', got {:?}", other),
            }
        }
    }

    #[test]
    fn docker_socket_mount_denied() {
        let e = engine(&["*"], &[], &[]);
        for cmd in [
            "docker run -v /var/run/docker.sock:/var/run/docker.sock ubuntu",
            "docker run -v /run/docker.sock:/var/run/docker.sock ubuntu",
            "docker run --volume=/var/run/docker.sock:/sock ubuntu",
        ] {
            match e.evaluate(cmd) {
                PolicyDecision::Deny(_) => {}
                other => panic!("Expected Deny for '{cmd}', got {:?}", other),
            }
        }
    }

    #[test]
    fn docker_proc_sys_dev_mount_denied() {
        let e = engine(&["*"], &[], &[]);
        for cmd in [
            "docker run -v /proc:/host_proc ubuntu",
            "docker run -v /sys:/host_sys ubuntu",
            "docker run -v /dev:/dev ubuntu",
            "docker run -v /boot:/boot ubuntu",
        ] {
            match e.evaluate(cmd) {
                PolicyDecision::Deny(_) => {}
                other => panic!("Expected Deny for '{cmd}', got {:?}", other),
            }
        }
    }

    #[test]
    fn docker_quoted_mount_denied() {
        let e = engine(&["*"], &[], &[]);
        // LLM may wrap paths in quotes — normalization should strip them
        for cmd in [
            r#"docker run -v "/etc":/host ubuntu"#,
            r#"docker run -v '/root':/host ubuntu"#,
            r#"docker run --mount type=bind,source="/",target=/host ubuntu"#,
        ] {
            match e.evaluate(cmd) {
                PolicyDecision::Deny(_) => {}
                other => panic!("Expected Deny for '{cmd}', got {:?}", other),
            }
        }
    }

    // ── Docker: safe operations ─────────────────────────────────────

    #[test]
    fn docker_safe_mounts_allowed() {
        let e = engine(&["*"], &[], &[]);
        for cmd in [
            "docker run -v /var/log:/logs ubuntu",
            "docker run -v /home/user/data:/data alpine",
            "docker run -v /opt/zymi/memory:/data alpine",
            "docker run -v /tmp/test:/tmp/test ubuntu",
            "docker run --mount type=bind,source=/opt/app,target=/app ubuntu",
        ] {
            assert_eq!(e.evaluate(cmd), PolicyDecision::Allow, "cmd: {cmd}");
        }
    }

    #[test]
    fn docker_normal_commands_allowed() {
        let e = engine(&["*"], &[], &[]);
        for cmd in [
            "docker ps -a",
            "docker run -d nginx",
            "docker run -p 8080:80 nginx",
            "docker stop my-container",
            "docker logs -f my-container",
            "docker exec my-container ls",
            "docker build -t myapp .",
            "docker compose up -d",
            "docker inspect my-container",
            "docker stats --no-stream",
        ] {
            assert_eq!(e.evaluate(cmd), PolicyDecision::Allow, "cmd: {cmd}");
        }
    }

    // ── Docker: mount path extraction unit tests ────────────────────

    #[test]
    fn extract_mount_sources_volume_flags() {
        let sources = extract_mount_sources("docker run -v /host:/container -v /tmp:/tmp ubuntu");
        assert_eq!(sources, vec!["/host", "/tmp"]);
    }

    #[test]
    fn extract_mount_sources_volume_equals() {
        let sources = extract_mount_sources("docker run --volume=/data:/data ubuntu");
        assert_eq!(sources, vec!["/data"]);
    }

    #[test]
    fn extract_mount_sources_mount_flag() {
        let sources = extract_mount_sources(
            "docker run --mount type=bind,source=/opt/app,target=/app ubuntu",
        );
        assert_eq!(sources, vec!["/opt/app"]);
    }

    #[test]
    fn extract_mount_sources_mount_src() {
        let sources =
            extract_mount_sources("docker run --mount type=bind,src=/data,target=/data ubuntu");
        assert_eq!(sources, vec!["/data"]);
    }

    #[test]
    fn sensitive_path_checks() {
        // Exact matches
        assert!(is_sensitive_path("/"));
        assert!(is_sensitive_path("/etc"));
        assert!(is_sensitive_path("/root"));
        assert!(is_sensitive_path("/var/run/docker.sock"));

        // Subpaths
        assert!(is_sensitive_path("/etc/shadow"));
        assert!(is_sensitive_path("/root/.ssh/authorized_keys"));
        assert!(is_sensitive_path("/proc/1/status"));

        // Safe paths
        assert!(!is_sensitive_path("/var/log"));
        assert!(!is_sensitive_path("/home/user"));
        assert!(!is_sensitive_path("/opt/zymi"));
        assert!(!is_sensitive_path("/tmp"));
        // Must not match prefix-substring without path separator
        assert!(!is_sensitive_path("/etcetera"));
        assert!(!is_sensitive_path("/rooted"));
    }
}
