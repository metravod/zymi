use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{self, Command, Stdio};

const PID_FILE: &str = ".zymi.pid";
const LOG_FILE: &str = "zymi.log"; // relative to memory_dir
const GITHUB_REPO: &str = "metravod/zymi";
const SYSTEMD_UNIT: &str = "/etc/systemd/system/zymi.service";
const SYSTEMD_ENV_FILE: &str = "/opt/zymi/.env";
const SYSTEMD_MEMORY_DIR: &str = "/opt/zymi/memory";

/// Returns true if a systemd service is installed and we're NOT already running inside it.
fn has_systemd_service() -> bool {
    Path::new(SYSTEMD_UNIT).exists() && std::env::var("INVOCATION_ID").is_err()
}

/// When a systemd service is installed, load its .env so that API keys
/// are available for interactive commands (login, setup, etc.).
/// Does NOT change MEMORY_DIR — interactive commands write locally,
/// then sync to the service dir via `sync_to_service()`.
pub fn apply_systemd_env() {
    if !has_systemd_service() {
        return;
    }
    let env_path = Path::new(SYSTEMD_ENV_FILE);
    if env_path.exists() {
        dotenvy::from_path(env_path).ok();
    }
}

/// Copy files from local memory_dir to the systemd service directory.
/// Used after login/setup to sync config to where the service reads from.
pub fn sync_to_service(memory_dir: &Path, files: &[&str]) {
    if !has_systemd_service() {
        return;
    }
    for file in files {
        let src = memory_dir.join(file);
        if !src.exists() {
            continue;
        }
        let dst = format!("{}/{}", SYSTEMD_MEMORY_DIR, file);
        let src_str = src.to_string_lossy();
        match Command::new("sudo")
            .args(["cp", &src_str, &dst])
            .status()
        {
            Ok(s) if s.success() => {
                let _ = Command::new("sudo")
                    .args(["chown", "zymi:zymi", &dst])
                    .status();
                println!("  Synced {file} → {dst}");
            }
            Ok(s) => {
                eprintln!(
                    "  Warning: failed to sync {file} (exit code {})",
                    s.code().unwrap_or(-1)
                );
            }
            Err(e) => {
                eprintln!("  Warning: failed to sync {file}: {e}");
            }
        }
    }
}

fn systemctl(args: &[&str]) -> bool {
    Command::new("sudo")
        .arg("systemctl")
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn memory_dir() -> String {
    std::env::var("MEMORY_DIR").unwrap_or_else(|_| "./memory".to_string())
}

fn log_path() -> std::path::PathBuf {
    Path::new(&memory_dir()).join(LOG_FILE)
}

fn read_pid() -> Option<u32> {
    fs::read_to_string(PID_FILE).ok()?.trim().parse().ok()
}

#[cfg(unix)]
fn is_pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[cfg(windows)]
fn is_pid_alive(pid: u32) -> bool {
    Command::new("tasklist")
        .args(["/fi", &format!("pid eq {pid}"), "/nh"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains("zymi"))
        .unwrap_or(false)
}

fn is_running() -> bool {
    read_pid().map(is_pid_alive).unwrap_or(false)
}

#[cfg(unix)]
fn kill_pid(pid: u32, signal: &str) {
    let _ = Command::new("kill")
        .args([signal, &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

pub fn start() {
    if has_systemd_service() {
        // Check if already running
        let already_running = Command::new("systemctl")
            .args(["is-active", "--quiet", "zymi.service"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if already_running {
            println!("Zymi is already running.");
            let _ = Command::new("systemctl")
                .args(["status", "--no-pager", "zymi.service"])
                .status();
            return;
        }

        println!("Starting zymi via systemd...");
        if systemctl(&["start", "zymi.service"]) {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let _ = Command::new("systemctl")
                .args(["status", "--no-pager", "zymi.service"])
                .status();
        } else {
            eprintln!("Failed to start. Check: sudo journalctl -u zymi -n 30");
            process::exit(1);
        }
        return;
    }

    // No systemd — direct daemon mode
    if is_running() {
        if let Some(pid) = read_pid() {
            println!("Zymi is already running (PID {pid}).");
        } else {
            println!("Zymi is already running.");
        }
        return;
    }

    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Failed to get executable path: {e}");
            process::exit(1);
        }
    };

    let log = log_path();
    let log_file = match fs::OpenOptions::new().create(true).append(true).open(&log) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Failed to open log file {}: {e}", log.display());
            process::exit(1);
        }
    };

    let log_stderr = match log_file.try_clone() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Failed to clone file handle: {e}");
            process::exit(1);
        }
    };

    let mut cmd = Command::new(&exe);
    cmd.arg("run")
        .stdin(Stdio::null())
        .stdout(log_file)
        .stderr(log_stderr);

    if std::env::var("RUST_LOG").is_err() {
        cmd.env("RUST_LOG", "info");
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    #[allow(clippy::zombie_processes)] // Intentional: daemon detaches from parent
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to start Zymi daemon: {e}");
            process::exit(1);
        }
    };
    let pid = child.id();

    if let Err(e) = fs::write(PID_FILE, pid.to_string()) {
        eprintln!("Failed to write PID file: {e}");
        process::exit(1);
    }

    // Wait for the process to either die or become ready (up to 5s)
    let log_display = log.display().to_string();
    for _ in 0..10 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if !is_pid_alive(pid) {
            eprintln!("Zymi failed to start. Check {log_display}");
            let _ = fs::remove_file(PID_FILE);
            process::exit(1);
        }
        // Log file being written to indicates successful startup
        if let Ok(meta) = fs::metadata(&log) {
            if meta.len() > 0 {
                break;
            }
        }
    }

    if is_pid_alive(pid) {
        println!("Zymi started (PID {pid}). Logs: {log_display}");
    } else {
        eprintln!("Zymi failed to start. Check {log_display}");
        let _ = fs::remove_file(PID_FILE);
        process::exit(1);
    }
}

pub fn stop() {
    if has_systemd_service() {
        println!("Stopping zymi via systemd...");
        if systemctl(&["stop", "zymi.service"]) {
            println!("Zymi stopped.");
        } else {
            eprintln!("Failed to stop zymi service. Check: sudo systemctl status zymi");
            process::exit(1);
        }
        return;
    }

    let pid = match read_pid() {
        Some(pid) if is_pid_alive(pid) => pid,
        _ => {
            println!("Zymi is not running.");
            let _ = fs::remove_file(PID_FILE);
            return;
        }
    };

    println!("Stopping Zymi (PID {pid})...");

    #[cfg(unix)]
    kill_pid(pid, "-TERM");

    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/pid", &pid.to_string(), "/f"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    for _ in 0..10 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if !is_pid_alive(pid) {
            break;
        }
    }

    #[cfg(unix)]
    if is_pid_alive(pid) {
        eprintln!("Force killing...");
        kill_pid(pid, "-KILL");
        std::thread::sleep(std::time::Duration::from_millis(500));
        if is_pid_alive(pid) {
            eprintln!("WARNING: Failed to kill process {pid}. It may still be running.");
        }
    }

    let _ = fs::remove_file(PID_FILE);
    println!("Zymi stopped.");
}

pub fn status() {
    if has_systemd_service() {
        let _ = Command::new("systemctl")
            .args(["status", "--no-pager", "zymi.service"])
            .status();
        return;
    }

    match read_pid() {
        Some(pid) if is_pid_alive(pid) => println!("Zymi is running (PID {pid})."),
        _ => {
            println!("Zymi is not running.");
            let _ = fs::remove_file(PID_FILE);
        }
    }
}

pub fn logs() {
    if has_systemd_service() {
        let _ = Command::new("journalctl")
            .args(["-u", "zymi", "-f", "-n", "50"])
            .status();
        return;
    }

    let log = log_path();

    if !log.exists() {
        println!("No log file yet ({}).", log.display());
        return;
    }

    let path_str = log.to_string_lossy().to_string();

    #[cfg(unix)]
    {
        let _ = Command::new("tail")
            .args(["-f", "-n", "50", &path_str])
            .status();
    }

    #[cfg(windows)]
    {
        let _ = Command::new("powershell")
            .args([
                "-Command",
                &format!("Get-Content '{}' -Wait -Tail 50", path_str),
            ])
            .status();
    }
}

fn detect_target() -> String {
    let os = match std::env::consts::OS {
        "linux" => "unknown-linux-musl",
        "macos" => "apple-darwin",
        "windows" => "pc-windows-msvc",
        other => {
            eprintln!("Unsupported OS: {other}");
            process::exit(1);
        }
    };

    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => {
            eprintln!("Unsupported architecture: {other}");
            process::exit(1);
        }
    };

    format!("{arch}-{os}")
}

/// RAII guard that removes a temp directory on drop.
struct TempDirGuard {
    path: std::path::PathBuf,
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub async fn update() {
    println!("Checking for latest release...");

    let client = reqwest::Client::new();
    let api_url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases/latest");

    let resp = match client
        .get(&api_url)
        .header("User-Agent", "zymi-updater")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Failed to check for updates: {e}");
            process::exit(1);
        }
    };

    let release: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("Failed to parse release info: {e}");
            process::exit(1);
        }
    };

    let latest_tag = match release["tag_name"].as_str() {
        Some(t) => t.to_string(),
        None => {
            eprintln!("No releases found.");
            process::exit(1);
        }
    };

    let latest_version = latest_tag.strip_prefix('v').unwrap_or(&latest_tag);
    let current_version = env!("CARGO_PKG_VERSION");

    if latest_version == current_version {
        println!("Already up to date (v{current_version}).");
        return;
    }

    println!("Current: v{current_version} -> Latest: {latest_tag}");
    println!();
    println!("  This will replace your binary with the official release from GitHub.");
    println!("  If you built from source with local changes, they will be lost.");
    println!("  To update from source: git pull && cargo install --path .");
    println!();
    print!("Proceed? [y/N] ");
    if std::io::stdout().flush().is_err() {
        eprintln!("Failed to flush stdout");
        process::exit(1);
    }

    let mut input = String::new();
    if std::io::stdin().read_line(&mut input).is_err() {
        eprintln!("Failed to read user input");
        process::exit(1);
    }
    if !matches!(input.trim(), "y" | "Y") {
        println!("Cancelled.");
        return;
    }

    let target = detect_target();
    let archive_name = if cfg!(windows) {
        format!("zymi-{latest_tag}-{target}.zip")
    } else {
        format!("zymi-{latest_tag}-{target}.tar.gz")
    };

    let download_url = format!(
        "https://github.com/{GITHUB_REPO}/releases/download/{latest_tag}/{archive_name}"
    );

    println!("Downloading {archive_name}...");

    let resp = match client.get(&download_url).send().await {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            eprintln!(
                "Download failed (HTTP {}). No release for your platform ({target})?",
                r.status()
            );
            process::exit(1);
        }
        Err(e) => {
            eprintln!("Download failed: {e}");
            process::exit(1);
        }
    };

    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Failed to download: {e}");
            process::exit(1);
        }
    };

    let tmp_dir = std::env::temp_dir().join("zymi-update");
    let _ = fs::remove_dir_all(&tmp_dir);
    if let Err(e) = fs::create_dir_all(&tmp_dir) {
        eprintln!("Failed to create temp directory: {e}");
        process::exit(1);
    }
    let _tmp_guard = TempDirGuard {
        path: tmp_dir.clone(),
    };

    let archive_path = tmp_dir.join(&archive_name);
    if let Err(e) = fs::write(&archive_path, &bytes) {
        eprintln!("Failed to write downloaded file: {e}");
        process::exit(1);
    }

    // Verify checksum if checksums file is available
    let checksums_url = format!(
        "https://github.com/{GITHUB_REPO}/releases/download/{latest_tag}/checksums.txt"
    );
    match client.get(&checksums_url).send().await {
        Ok(r) if r.status().is_success() => {
            if let Ok(checksums_text) = r.text().await {
                use sha2::Digest;
                let mut hasher = sha2::Sha256::new();
                hasher.update(&bytes);
                let hash = format!("{:x}", hasher.finalize());
                let expected = checksums_text
                    .lines()
                    .find(|line| line.contains(&archive_name))
                    .and_then(|line| line.split_whitespace().next());
                match expected {
                    Some(expected_hash) if expected_hash == hash => {
                        println!("Checksum verified (SHA-256).");
                    }
                    Some(expected_hash) => {
                        eprintln!(
                            "Checksum mismatch!\n  Expected: {}\n  Got:      {}",
                            expected_hash, hash
                        );
                        process::exit(1);
                    }
                    None => {
                        println!("Warning: archive not found in checksums.txt, skipping verification.");
                    }
                }
            }
        }
        _ => {
            println!("Warning: no checksums.txt in release, skipping verification.");
        }
    }

    // Extract
    #[cfg(unix)]
    {
        let status = match Command::new("tar")
            .args([
                "xzf",
                &archive_path.to_string_lossy(),
                "-C",
                &tmp_dir.to_string_lossy(),
            ])
            .status()
        {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Failed to run tar: {e}");
                process::exit(1);
            }
        };
        if !status.success() {
            eprintln!("Failed to extract archive.");
            process::exit(1);
        }
    }

    #[cfg(windows)]
    {
        let status = match Command::new("powershell")
            .args([
                "-Command",
                &format!(
                    "Expand-Archive -Path '{}' -DestinationPath '{}' -Force",
                    archive_path.display(),
                    tmp_dir.display()
                ),
            ])
            .status()
        {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Failed to extract archive: {e}");
                process::exit(1);
            }
        };
        if !status.success() {
            eprintln!("Failed to extract archive.");
            process::exit(1);
        }
    }

    let bin_name = if cfg!(windows) { "zymi.exe" } else { "zymi" };
    let new_bin = tmp_dir
        .join(format!("zymi-{latest_tag}-{target}"))
        .join(bin_name);

    if !new_bin.exists() {
        eprintln!("Binary not found in archive.");
        process::exit(1);
    }

    // Replace binary — try direct write, fall back to sudo on permission denied
    let current_exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Failed to get executable path: {e}");
            process::exit(1);
        }
    };

    let temp_exe = current_exe.with_extension("update-tmp");
    match fs::copy(&new_bin, &temp_exe) {
        Ok(_) => {
            // Direct write succeeded
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Err(e) =
                    fs::set_permissions(&temp_exe, fs::Permissions::from_mode(0o755))
                {
                    eprintln!("Failed to set permissions: {e}");
                    let _ = fs::remove_file(&temp_exe);
                    process::exit(1);
                }
            }
            if let Err(e) = fs::rename(&temp_exe, &current_exe) {
                eprintln!("Failed to replace binary: {e}");
                let _ = fs::remove_file(&temp_exe);
                process::exit(1);
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            // Fall back to sudo
            println!(
                "Root permissions required to update {}.",
                current_exe.display()
            );
            let status = match Command::new("sudo")
                .args(["cp", "-f"])
                .arg(&new_bin)
                .arg(&current_exe)
                .status()
            {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("Failed to run sudo: {e}");
                    process::exit(1);
                }
            };
            if !status.success() {
                eprintln!("Failed to copy binary with sudo.");
                process::exit(1);
            }
            let _ = Command::new("sudo")
                .args(["chmod", "755"])
                .arg(&current_exe)
                .status();
        }
        Err(e) => {
            eprintln!("Failed to copy binary: {e}");
            process::exit(1);
        }
    }

    println!("Updated to {latest_tag}.");

    if is_running() {
        println!("Restart the daemon to use the new version: zymi stop && zymi");
    }
}
