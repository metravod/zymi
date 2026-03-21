use std::io::{self, Write};
use std::path::Path;
#[cfg(target_os = "linux")]
use std::process::Command;

use crate::core::config::{load_models_config, save_models_config, ModelEntry, ModelsConfig, ProviderType};

/// Default AGENT.md content for new installations.
const DEFAULT_AGENT_PROMPT: &str = r#"# Role

You are **zymi**, an AI assistant running on the user's server.

Your job is to complete tasks reliably, safely, and with minimal friction.

- Be concise and action-oriented.
- Use tools deliberately.
- Avoid unnecessary risk.
- Do not over-explain.

---

# Tools

Use the simplest tool that can complete the task.

**Never ask permission before calling a tool.** Call tools directly — the approval system handles confirmation automatically.

## `think`
Use for structured planning on multi-step or ambiguous tasks.

## `execute_shell`
Full access to the user's shell. Install packages, run scripts, use any CLI tool.

## `read_memory` / `write_memory`
Long-term context and durable notes.

## `ask_user`
Use when you need clarification or a choice to proceed.

## `web_search` / `web_scrape`
Use when external or up-to-date information is needed.

---

# Safety

- Never perform destructive or irreversible actions without explicit user confirmation.
- If a command may affect production systems, credentials, or user data, be extra cautious.
- When in doubt, inspect first and ask before changing.

---

# Response Style

- Be brief and direct.
- Respond in the user's language unless asked otherwise.
- After completion, summarize the result clearly.
"#;

/// Default .gitignore for the memory directory.
const DEFAULT_MEMORY_GITIGNORE: &str = "\
conversations.db
conversations.db-wal
conversations.db-shm
.heartbeat
zymi.log
tool_embeddings.json
auth.json
";

/// Scaffold default files in the memory directory (idempotent).
pub fn init_memory_dir(memory_dir: &Path) {
    // AGENT.md — main system prompt
    let agent_path = memory_dir.join("AGENT.md");
    if !agent_path.exists() {
        if let Err(e) = std::fs::write(&agent_path, DEFAULT_AGENT_PROMPT) {
            log::warn!("Failed to create default AGENT.md: {e}");
        }
    }

    // .gitignore — exclude transient/binary files
    let gitignore_path = memory_dir.join(".gitignore");
    if !gitignore_path.exists() {
        if let Err(e) = std::fs::write(&gitignore_path, DEFAULT_MEMORY_GITIGNORE) {
            log::warn!("Failed to create default .gitignore: {e}");
        }
    }

    // subagents/ directory
    let subagents_dir = memory_dir.join("subagents");
    let _ = std::fs::create_dir_all(&subagents_dir);
}

/// Run the git backup setup step standalone (`zymi git`).
pub fn run_git_setup(memory_dir: &Path) {
    step_git_backup(memory_dir);
}

/// Check whether the setup wizard should run.
pub fn needs_setup(memory_dir: &Path) -> bool {
    // If models.json exists with entries, we're configured
    if let Ok(content) = std::fs::read_to_string(memory_dir.join("models.json")) {
        if let Ok(config) = serde_json::from_str::<ModelsConfig>(&content) {
            if !config.models.is_empty() {
                return false;
            }
        }
    }

    // Check if OAuth tokens are present
    if crate::auth::storage::load_tokens(memory_dir).is_some() {
        return false;
    }

    // No valid models.json — check if env fallback is available
    let has_key = std::env::var("OPENAI_API_KEY")
        .map(|k| !k.is_empty())
        .unwrap_or(false);

    !has_key
}

/// Run the interactive setup wizard.
/// Returns `true` if setup completed, `false` if the user chose manual config.
pub fn run_setup(memory_dir: &Path) -> bool {
    print_welcome_banner();

    println!("  How would you like to configure Zymi?");
    println!();
    println!("  \x1b[1m[1]\x1b[0m Guided setup \x1b[2m(recommended)\x1b[0m");
    println!("  \x1b[1m[2]\x1b[0m I'll create .env myself");
    println!();

    if read_choice("  > ", &["1", "2"], "1") == "2" {
        println!();
        println!("  Create a \x1b[1m.env\x1b[0m file in the current directory.");
        println!("  See \x1b[1m.env.example\x1b[0m for the available variables.");
        println!("  Then run zymi again.");
        println!();
        return false;
    }

    let mut env_vars: Vec<(String, String)> = Vec::new();

    let model_entry = step_model(&mut env_vars);
    step_telegram(&mut env_vars);
    step_tavily(&mut env_vars);
    step_firecrawl(&mut env_vars);
    let browserbase_configured = step_browserbase(&mut env_vars);
    step_git_backup(memory_dir);

    // Persist configuration
    write_env_file(&env_vars);

    let config = ModelsConfig {
        models: vec![model_entry],
        settings: Default::default(),
    };
    save_models_config(memory_dir, &config);

    if browserbase_configured {
        add_browserbase_mcp(memory_dir);
    }
    create_browserbase_subagent(memory_dir);

    // Load into current process so startup can proceed
    for (key, value) in &env_vars {
        std::env::set_var(key, value);
    }

    // Offer systemd service installation (Linux only)
    #[cfg(target_os = "linux")]
    step_systemd_service();

    print_complete_banner();
    true
}

/// Append or update a single env variable in .env file.
/// Used by the AddModel flow in CLI.
pub fn append_env_var(key: &str, value: &str) {
    let mut entries = load_env_entries();

    if let Some(entry) = entries.iter_mut().find(|(k, _)| k == key) {
        entry.1 = value.to_string();
    } else {
        entries.push((key.to_string(), value.to_string()));
    }

    save_env_entries(&entries);
}

/// Sync ChatGPT OAuth models from the API response into models.json.
/// Adds new models, preserves existing non-OAuth models. Returns the number of models added.
pub fn sync_chatgpt_models(memory_dir: &Path, api_models: &[crate::auth::login::ChatgptModel]) -> usize {
    let mut config = load_models_config(memory_dir);

    // Remove existing ChatGPT OAuth entries (they'll be replaced by fresh data)
    config.models.retain(|m| m.provider != ProviderType::ChatgptOauth);

    let no_default = !config.models.iter().any(|m| m.is_default);
    let mut count = 0;

    for (i, model) in api_models.iter().enumerate() {
        let name = model
            .display_name
            .clone()
            .unwrap_or_else(|| model.slug.clone());

        config.models.push(ModelEntry {
            id: model.slug.clone(),
            name,
            provider: ProviderType::ChatgptOauth,
            api_key_env: String::new(),
            base_url: None,
            api_key: None,
            is_default: no_default && i == 0, // first model is default if no other default exists
            input_price_per_1m: None,
            output_price_per_1m: None,
        });
        count += 1;
    }

    save_models_config(memory_dir, &config);
    count
}

/// Fallback: ensure at least one ChatGPT OAuth model entry exists in models.json.
pub fn ensure_chatgpt_model(memory_dir: &Path) {
    let config = load_models_config(memory_dir);

    let already_has = config
        .models
        .iter()
        .any(|m| m.provider == ProviderType::ChatgptOauth);

    if already_has {
        return;
    }

    let fallback = crate::auth::login::ChatgptModel {
        slug: "o4-mini".to_string(),
        display_name: Some("o4-mini".to_string()),
        priority: Some(0),
        visibility: None,
    };
    sync_chatgpt_models(memory_dir, &[fallback]);
}

// ============================================================
// Setup steps
// ============================================================

fn step_model(env_vars: &mut Vec<(String, String)>) -> ModelEntry {
    println!();
    println!("  \x1b[1;36m-- Step 1 : AI Model --\x1b[0m");
    println!();
    println!("  Zymi needs an AI model to think and respond.");
    println!("  Which provider do you want to connect?");
    println!();
    println!("  \x1b[1m[1]\x1b[0m OpenAI            \x1b[2m(GPT-4o, GPT-4.1, o3, ...)\x1b[0m");
    println!("  \x1b[1m[2]\x1b[0m Anthropic         \x1b[2m(Claude Sonnet, Opus, Haiku, ...)\x1b[0m");
    println!("  \x1b[1m[3]\x1b[0m OpenAI-compatible \x1b[2m(Ollama, Together, Azure, ...)\x1b[0m");
    println!("  \x1b[1m[4]\x1b[0m ChatGPT Plus/Pro  \x1b[2m(OAuth — use your subscription)\x1b[0m");
    println!();

    let provider_choice = read_choice("  > ", &["1", "2", "3", "4"], "1");

    if provider_choice == "4" {
        println!();
        println!("  Run \x1b[1mzymi login\x1b[0m after setup to authenticate with OpenAI.");
        println!();

        println!("  \x1b[2mAvailable: o4-mini (Plus default), o3, gpt-4.1\x1b[0m");
        let model_id = prompt(
            "  Model ID \x1b[2m[o4-mini]\x1b[0m: ",
            Some("o4-mini"),
        );
        let display_name = prompt(
            "  Display name \x1b[2m[ChatGPT Plus]\x1b[0m: ",
            Some("ChatGPT Plus"),
        );

        println!();
        println!("  \x1b[1;32m+\x1b[0m Model configured! Don't forget to run \x1b[1mzymi login\x1b[0m.");

        return ModelEntry {
            id: model_id,
            name: display_name,
            provider: ProviderType::ChatgptOauth,
            api_key_env: String::new(),
            base_url: None,
            api_key: None,
            is_default: true,
            input_price_per_1m: None,
            output_price_per_1m: None,
        };
    }

    let (provider_type, default_model, default_env) = match provider_choice.as_str() {
        "2" => (
            ProviderType::Anthropic,
            "claude-sonnet-4-20250514",
            "ANTHROPIC_API_KEY",
        ),
        "3" => (
            ProviderType::OpenaiCompatible,
            "gpt-4.1-mini",
            "OPENAI_API_KEY",
        ),
        _ => (
            ProviderType::OpenaiCompatible,
            "gpt-4.1-mini",
            "OPENAI_API_KEY",
        ),
    };

    println!();
    let model_id = prompt(
        &format!("  Model ID \x1b[2m[{}]\x1b[0m: ", default_model),
        Some(default_model),
    );

    let display_name = prompt(
        &format!("  Display name \x1b[2m[{}]\x1b[0m: ", model_id),
        Some(&model_id),
    );

    let base_url = if provider_choice == "3" {
        println!();
        println!("  \x1b[2mFor local models (Ollama): http://localhost:11434/v1\x1b[0m");
        let url = prompt("  Base URL \x1b[2m(empty to skip)\x1b[0m: ", None);
        if url.is_empty() { None } else { Some(url) }
    } else {
        None
    };

    println!();
    let api_key = prompt("  API Key: ", None);

    let env_var_name = prompt(
        &format!("  Save as env variable \x1b[2m[{}]\x1b[0m: ", default_env),
        Some(default_env),
    );

    env_vars.push((env_var_name.clone(), api_key));

    println!();
    println!("  \x1b[1;32m+\x1b[0m Model configured!");

    ModelEntry {
        id: model_id,
        name: display_name,
        provider: provider_type,
        api_key_env: env_var_name,
        base_url,
        api_key: None,
        is_default: true,
        input_price_per_1m: None,
        output_price_per_1m: None,
    }
}

fn step_telegram(env_vars: &mut Vec<(String, String)>) {
    println!();
    println!("  \x1b[1;36m-- Step 2 : Telegram Bot (optional) --\x1b[0m");
    println!();
    println!("  Lets you chat with Zymi from your phone, anywhere.");
    println!("  You need a bot token from \x1b[1m@BotFather\x1b[0m \x1b[2m— https://t.me/BotFather\x1b[0m");
    println!();

    if !confirm("  Set up Telegram?", false) {
        println!("  \x1b[2mSkipped.\x1b[0m");
        return;
    }

    println!();
    let token = prompt("  Bot token: ", None);
    env_vars.push(("TELOXIDE_TOKEN".to_string(), token));

    println!();
    println!("  To find your user ID, message \x1b[1m@userinfobot\x1b[0m \x1b[2m— https://t.me/userinfobot\x1b[0m");
    println!("  \x1b[2mSeparate multiple IDs with commas.\x1b[0m");
    println!();
    let user_ids = prompt("  Allowed user ID(s): ", None);
    env_vars.push(("ALLOWED_USERS".to_string(), user_ids));

    println!();
    println!("  \x1b[1;32m+\x1b[0m Telegram configured!");
}

fn step_tavily(env_vars: &mut Vec<(String, String)>) {
    println!();
    println!("  \x1b[1;36m-- Step 3 : Web Search — Tavily (optional) --\x1b[0m");
    println!();
    println!("  Gives Zymi the ability to search the internet.");
    println!("  \x1b[2mFree API key: https://tavily.com\x1b[0m");
    println!();

    if !confirm("  Set up Tavily?", false) {
        println!("  \x1b[2mSkipped.\x1b[0m");
        return;
    }

    println!();
    let key = prompt("  Tavily API Key: ", None);
    env_vars.push(("TAVILY_API_KEY".to_string(), key));
    println!();
    println!("  \x1b[1;32m+\x1b[0m Tavily configured!");
}

fn step_firecrawl(env_vars: &mut Vec<(String, String)>) {
    println!();
    println!("  \x1b[1;36m-- Step 4 : Web Scraping — Firecrawl (optional) --\x1b[0m");
    println!();
    println!("  Lets Zymi read and understand web pages you link.");
    println!("  \x1b[2mAPI key: https://firecrawl.dev\x1b[0m");
    println!();

    if !confirm("  Set up Firecrawl?", false) {
        println!("  \x1b[2mSkipped.\x1b[0m");
        return;
    }

    println!();
    let key = prompt("  Firecrawl API Key: ", None);
    env_vars.push(("FIRECRAWL_API_KEY".to_string(), key));
    println!();
    println!("  \x1b[1;32m+\x1b[0m Firecrawl configured!");
}

/// Returns `true` if Browserbase was successfully configured.
fn step_browserbase(env_vars: &mut Vec<(String, String)>) -> bool {
    println!();
    println!("  \x1b[1;36m-- Step 5 : Browser — Browserbase (optional) --\x1b[0m");
    println!();
    println!("  Gives Zymi a real browser to surf the web: navigate pages,");
    println!("  click buttons, fill forms, read JavaScript-rendered content.");
    println!("  \x1b[2mAPI key + project ID: https://browserbase.com\x1b[0m");
    println!();

    if !confirm("  Set up Browserbase?", false) {
        println!("  \x1b[2mSkipped.\x1b[0m");
        return false;
    }

    if !check_npx() {
        println!();
        println!("  \x1b[1;33m!\x1b[0m Browserbase MCP server requires Node.js (for npx).");
        println!("  \x1b[2mNode.js includes npm and npx.\x1b[0m");
        println!();
        if confirm("  Install Node.js now?", true) {
            if !install_node() {
                println!();
                println!("  \x1b[1;31m!\x1b[0m Could not install Node.js automatically.");
                println!("  Install manually: \x1b[1mhttps://nodejs.org\x1b[0m");
                println!("  Then run \x1b[1mzymi --setup\x1b[0m again.");
                println!();
                println!("  \x1b[2mSkipping Browserbase for now.\x1b[0m");
            }
        } else {
            println!();
            println!("  Install Node.js later: \x1b[1mhttps://nodejs.org\x1b[0m");
            println!("  Then run \x1b[1mzymi --setup\x1b[0m to configure Browserbase.");
            println!();
            println!("  \x1b[2mSkipping Browserbase for now.\x1b[0m");
        }
    }

    // Re-check after possible install
    if !check_npx() {
        return false;
    }

    println!();
    let key = prompt("  Browserbase API Key: ", None);
    env_vars.push(("BROWSERBASE_API_KEY".to_string(), key));

    let project_id = prompt("  Browserbase Project ID: ", None);
    env_vars.push(("BROWSERBASE_PROJECT_ID".to_string(), project_id));

    println!();
    println!("  \x1b[1;32m+\x1b[0m Browserbase configured!");
    true
}

fn step_git_backup(memory_dir: &Path) {
    println!();
    println!("  \x1b[1;36m-- Memory Backup — Git --\x1b[0m");
    println!();
    println!("  Zymi can back up its memory (prompts, configs, notes) to a");
    println!("  private git repo. Every change is auto-committed and pushed.");
    println!();

    let is_repo = memory_dir.join(".git").is_dir();

    if !is_repo {
        println!("  \x1b[1m[1]\x1b[0m Set up git backup");
        println!("  \x1b[1m[2]\x1b[0m Skip");
        println!();

        let choice = read_choice("  > ", &["1", "2"], "1");

        match choice.as_str() {
            "2" => {
                println!("  \x1b[2mSkipped.\x1b[0m");
                return;
            }
            _ => {
                // "1" — init fresh
                match crate::git_sync::GitSync::init_repo(memory_dir) {
                    Ok(()) => println!("  \x1b[1;32m+\x1b[0m Initialized git repo in memory/"),
                    Err(e) => {
                        println!("  \x1b[1;31m!\x1b[0m Failed to init repo: {e}");
                        println!("  \x1b[2mSkipping git setup.\x1b[0m");
                        return;
                    }
                }
            }
        }
    } else {
        println!("  \x1b[2mMemory directory is already a git repo.\x1b[0m");
    }

    println!();
    println!("  \x1b[2mCreate a private repo on GitHub/GitLab and paste the URL.\x1b[0m");
    println!("  \x1b[2mLeave empty to skip remote (local commits only).\x1b[0m");
    println!();
    let remote_url = prompt("  Remote URL: ", None);

    if !remote_url.is_empty() {
        match crate::git_sync::GitSync::add_remote(memory_dir, &remote_url) {
            Ok(()) => println!("  \x1b[1;32m+\x1b[0m Remote configured and pushed!"),
            Err(e) => println!("  \x1b[1;33m!\x1b[0m Remote push failed: {e}\n  \x1b[2mYou can add it later: git -C memory remote add origin <url>\x1b[0m"),
        }
    } else {
        println!("  \x1b[2mNo remote — local commits only. Add one later if needed.\x1b[0m");
    }

    println!();
    println!("  \x1b[1;32m+\x1b[0m Git backup configured!");
}

// ============================================================
// Step: systemd service (Linux only)
// ============================================================

#[cfg(target_os = "linux")]
fn step_systemd_service() {
    println!();
    println!("  \x1b[1;36m─── Systemd Service ───────────────────────\x1b[0m");
    println!();
    println!("  Install zymi as a systemd service?");
    println!("  This will create a dedicated system user, configure");
    println!("  sudoers for healthchecks, and enable auto-start.");
    println!("  \x1b[2mRequires sudo.\x1b[0m");
    println!();

    if !confirm("  Install systemd service?", false) {
        println!("  \x1b[2mSkipped. You can re-run `zymi setup` later.\x1b[0m");
        return;
    }

    // Check if we can get sudo
    let sudo_check = Command::new("sudo").args(["-n", "true"]).output();
    let has_sudo = sudo_check.map(|o| o.status.success()).unwrap_or(false);

    if !has_sudo {
        println!();
        println!("  \x1b[2mSudo requires a password. Re-run setup with sudo:\x1b[0m");
        println!("  \x1b[1msudo zymi setup\x1b[0m");
        return;
    }

    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            println!("  \x1b[31mCannot determine working directory: {e}\x1b[0m");
            return;
        }
    };

    let zumi_user = "zymi";
    let zumi_group = "zymi";

    // Create system user & group
    if !user_exists(zumi_user) {
        run_sudo(&["groupadd", "--system", zumi_group]);
        run_sudo(&["useradd", "--system",
            "--gid", zumi_group,
            "--shell", "/usr/sbin/nologin",
            "--home-dir", "/opt/zymi",
            "--no-create-home",
            zumi_user,
        ]);
        println!("  \x1b[1;32m+\x1b[0m Created system user '{zumi_user}'");
    } else {
        println!("  \x1b[2m  User '{zumi_user}' already exists\x1b[0m");
    }

    // Add to supplementary groups
    for group in &["docker", "adm", "systemd-journal"] {
        if group_exists(group) {
            run_sudo(&["usermod", "-aG", group, zumi_user]);
        }
    }

    // Create directory structure
    run_sudo(&["mkdir", "-p", "/opt/zymi/memory"]);
    for sub in &["baseline", "subagents", "evals", "eval_results", "workflow_scripts", "workflow_traces"] {
        run_sudo(&["mkdir", "-p", &format!("/opt/zymi/memory/{sub}")]);
    }

    // Copy current config to /opt/zymi
    let env_src = cwd.join(".env");
    if env_src.exists() {
        run_sudo(&["cp", &env_src.to_string_lossy(), "/opt/zymi/.env"]);
        run_sudo(&["chmod", "600", "/opt/zymi/.env"]);
    }

    // Copy models.json and auth.json if present
    let memory_dir_path = cwd.join("memory");
    for file in &["models.json", "auth.json", "AGENT.md"] {
        let src = memory_dir_path.join(file);
        if src.exists() {
            run_sudo(&["cp", &src.to_string_lossy(), &format!("/opt/zymi/memory/{file}")]);
        }
    }

    // Set ownership
    run_sudo(&["chown", "-R", &format!("{zumi_user}:{zumi_group}"), "/opt/zymi"]);
    run_sudo(&["chmod", "750", "/opt/zymi"]);
    run_sudo(&["chmod", "700", "/opt/zymi/memory"]);

    // Sudoers
    let sudoers = include_str!("../assets/sudoers.conf");
    let sudoers_path = "/etc/sudoers.d/zymi";
    if write_via_sudo(sudoers_path, sudoers) {
        run_sudo(&["chmod", "440", sudoers_path]);
        let check = Command::new("sudo")
            .args(["visudo", "-c", "-f", sudoers_path])
            .output();
        if check.map(|o| o.status.success()).unwrap_or(false) {
            println!("  \x1b[1;32m+\x1b[0m Sudoers configured");
        } else {
            println!("  \x1b[33m!\x1b[0m Sudoers syntax error, removing");
            run_sudo(&["rm", "-f", sudoers_path]);
        }
    }

    // systemd unit
    let unit = include_str!("../assets/zymi.service");
    if write_via_sudo("/etc/systemd/system/zymi.service", unit) {
        run_sudo(&["systemctl", "daemon-reload"]);
        run_sudo(&["systemctl", "enable", "zymi.service"]);
        println!("  \x1b[1;32m+\x1b[0m Systemd service installed and enabled");
    }

    println!();
    println!("  Start with: \x1b[1msudo systemctl start zymi\x1b[0m");
    println!("  Logs:       \x1b[1msudo journalctl -u zymi -f\x1b[0m");
}

#[cfg(target_os = "linux")]
fn run_sudo(args: &[&str]) -> bool {
    Command::new("sudo")
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn write_via_sudo(path: &str, content: &str) -> bool {
    let mut child = match Command::new("sudo")
        .args(["tee", path])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    if let Some(ref mut stdin) = child.stdin {
        let _ = io::Write::write_all(stdin, content.as_bytes());
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn user_exists(name: &str) -> bool {
    Command::new("id").arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn group_exists(name: &str) -> bool {
    Command::new("getent").args(["group", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ============================================================
// UI helpers
// ============================================================

fn print_welcome_banner() {
    println!();
    println!("  \x1b[1;36m+------------------------------------------+\x1b[0m");
    println!("  \x1b[1;36m|\x1b[0m                                          \x1b[1;36m|\x1b[0m");
    println!("  \x1b[1;36m|\x1b[0m   \x1b[1mWelcome to Zymi!\x1b[0m                       \x1b[1;36m|\x1b[0m");
    println!("  \x1b[1;36m|\x1b[0m   Let's get you set up.                  \x1b[1;36m|\x1b[0m");
    println!("  \x1b[1;36m|\x1b[0m                                          \x1b[1;36m|\x1b[0m");
    println!("  \x1b[1;36m+------------------------------------------+\x1b[0m");
    println!();
}

fn print_complete_banner() {
    println!();
    println!("  \x1b[1;36m+------------------------------------------+\x1b[0m");
    println!("  \x1b[1;36m|\x1b[0m                                          \x1b[1;36m|\x1b[0m");
    println!("  \x1b[1;36m|\x1b[0m   \x1b[1;32mSetup complete!\x1b[0m                        \x1b[1;36m|\x1b[0m");
    println!("  \x1b[1;36m|\x1b[0m   .env and models.json created.          \x1b[1;36m|\x1b[0m");
    println!("  \x1b[1;36m|\x1b[0m                                          \x1b[1;36m|\x1b[0m");
    println!("  \x1b[1;36m|\x1b[0m   Run `zymi` for daemon mode             \x1b[1;36m|\x1b[0m");
    println!("  \x1b[1;36m|\x1b[0m   or  `zymi cli` for interactive TUI.    \x1b[1;36m|\x1b[0m");
    println!("  \x1b[1;36m|\x1b[0m                                          \x1b[1;36m|\x1b[0m");
    println!("  \x1b[1;36m+------------------------------------------+\x1b[0m");
    println!();
}

fn prompt(text: &str, default: Option<&str>) -> String {
    print!("{}", text);
    io::stdout().flush().unwrap();

    let mut buf = String::new();
    io::stdin().read_line(&mut buf).unwrap();
    let trimmed = buf.trim();

    if trimmed.is_empty() {
        default.unwrap_or("").to_string()
    } else {
        trimmed.to_string()
    }
}

fn read_choice(text: &str, options: &[&str], default: &str) -> String {
    loop {
        let input = prompt(text, Some(default));
        if options.contains(&input.as_str()) {
            return input;
        }
        println!("  Please choose: {}", options.join(", "));
    }
}

fn confirm(text: &str, default: bool) -> bool {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    let input = prompt(&format!("{} {}: ", text, hint), None);
    match input.to_lowercase().as_str() {
        "y" | "yes" => true,
        "n" | "no" => false,
        _ => default,
    }
}

// ============================================================
// File I/O
// ============================================================

fn load_env_entries() -> Vec<(String, String)> {
    let mut entries = Vec::new();
    if let Ok(content) = std::fs::read_to_string(".env") {
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = trimmed.split_once('=') {
                entries.push((key.to_string(), value.to_string()));
            }
        }
    }
    entries
}

fn save_env_entries(entries: &[(String, String)]) {
    let content: String = entries
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("\n");

    std::fs::write(".env", content + "\n").expect("Failed to write .env file");

    // Restrict .env permissions to owner-only (contains API keys)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(".env", std::fs::Permissions::from_mode(0o600));
    }
}

fn write_env_file(vars: &[(String, String)]) {
    let mut entries = load_env_entries();

    for (key, value) in vars {
        if let Some(entry) = entries.iter_mut().find(|(k, _)| k == key) {
            entry.1 = value.clone();
        } else {
            entries.push((key.clone(), value.clone()));
        }
    }

    save_env_entries(&entries);
}

// ============================================================
// External tool helpers
// ============================================================

fn check_npx() -> bool {
    std::process::Command::new("npx")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn install_node() -> bool {
    let strategies: &[(&str, &[&str], &str)] = if cfg!(target_os = "macos") {
        &[("brew", &["install", "node"], "Homebrew")]
    } else if cfg!(target_os = "linux") {
        &[
            ("apt-get", &["install", "-y", "nodejs", "npm"], "apt"),
            ("dnf", &["install", "-y", "nodejs", "npm"], "dnf"),
            ("pacman", &["-S", "--noconfirm", "nodejs", "npm"], "pacman"),
        ]
    } else {
        &[]
    };

    for (cmd, args, name) in strategies {
        let has_pm = std::process::Command::new("which")
            .arg(cmd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if !has_pm {
            continue;
        }

        println!();
        println!("  Found \x1b[1m{}\x1b[0m, installing Node.js...", name);

        let status = std::process::Command::new(cmd).args(*args).status();

        match status {
            Ok(s) if s.success() => {
                println!("  \x1b[1;32m+\x1b[0m Node.js installed!");
                return check_npx();
            }
            _ => {
                println!("  \x1b[1;33m!\x1b[0m {} failed, trying next option...", name);
            }
        }
    }

    false
}

fn add_browserbase_mcp(memory_dir: &Path) {
    let mcp_path = memory_dir.join("mcp.json");

    let mut config: serde_json::Value = if let Ok(content) = std::fs::read_to_string(&mcp_path) {
        serde_json::from_str(&content).unwrap_or_else(|_| {
            serde_json::json!({ "mcpServers": {} })
        })
    } else {
        serde_json::json!({ "mcpServers": {} })
    };

    let servers = config
        .get_mut("mcpServers")
        .and_then(|s| s.as_object_mut());

    if let Some(servers) = servers {
        if !servers.contains_key("browserbase") {
            servers.insert(
                "browserbase".to_string(),
                serde_json::json!({
                    "command": "npx",
                    "args": ["-y", "@browserbasehq/mcp-server-browserbase"],
                    "env": {}
                }),
            );
        }
    }

    if let Ok(content) = serde_json::to_string_pretty(&config) {
        let _ = std::fs::write(&mcp_path, content);
    }
}

fn create_browserbase_subagent(memory_dir: &Path) {
    let subagents_dir = memory_dir.join("subagents");
    let _ = std::fs::create_dir_all(&subagents_dir);

    let path = subagents_dir.join("browserbase_setup.md");
    if path.exists() {
        return;
    }

    let prompt = r#"# Browserbase Setup Agent

You are a setup agent responsible for configuring the Browserbase MCP server for Zymi.

## When to use
- User wants to set up or fix Browserbase browser integration
- Browserbase API keys exist in environment but MCP server is not configured
- Browserbase MCP tools are not working

## Steps

1. **Check environment variables:**
   - Run: `echo $BROWSERBASE_API_KEY | head -c 5` (verify key exists, don't print full key)
   - Run: `echo $BROWSERBASE_PROJECT_ID`
   - If missing, tell the user to add them to .env and restart

2. **Check if npx is available:**
   - Run: `which npx`
   - If not found, tell the user to install Node.js (https://nodejs.org)

3. **Update mcp.json:**
   - Read the current config with read_memory (filename: mcp.json)
   - Add the browserbase server entry if missing:
     ```json
     "browserbase": {
       "command": "npx",
       "args": ["-y", "@browserbasehq/mcp-server-browserbase"],
       "env": {}
     }
     ```
   - Write the updated config with write_memory

4. **Verify MCP server can start:**
   - Run: `npx -y @browserbasehq/mcp-server-browserbase --help` (or similar quick check)

5. **Report result:**
   - If everything is OK, tell the user to restart Zymi for the browser tools to become available
   - List the tools that will be available: navigate, click, type, screenshot, get_text, etc.
"#;

    let _ = std::fs::write(&path, prompt);
}
