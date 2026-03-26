mod audit;
mod auth;
mod connectors;
mod core;
mod daemon;
mod eval;
mod events;
mod git_sync;
mod mcp;
mod policy;
mod sandbox;
mod scheduler;
mod setup;
mod storage;
mod task_registry;
mod tools;
mod workflow;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::core::agent::{Agent, MonitorConfig};
use crate::core::approval::{new_shared_approval_handler, SharedApprovalHandler};
use crate::core::config::{self, ModelsConfig};
use crate::core::debug_provider::{DebugEvent, DebugProvider};
use crate::core::langfuse::{LangfuseClient, LangfuseConfig};
use crate::core::provider_manager::ProviderManager;
use crate::core::tool_selector::ToolSelector;
use crate::core::LlmProvider;
use crate::git_sync::GitSync;
use crate::mcp::McpManager;
use crate::storage::sqlite_storage::SqliteStorage;
use crate::tools::ask_user::AskUserTool;
use crate::tools::create_sub_agent::CreateSubAgentTool;
use crate::tools::current_time::CurrentTimeTool;
use crate::tools::eval_gen::GenerateEvalsTool;
use crate::tools::eval_run::RunEvalsTool;
use crate::tools::manage_mcp::ManageMcpTool;
use crate::tools::memory::{ReadMemoryTool, WriteMemoryTool};
use crate::tools::policy::ManagePolicyTool;
use crate::tools::planning::PlanningTool;
use crate::tools::run_code::RunCodeTool;
use crate::tools::schedule::ManageScheduleTool;
use crate::tools::shell::ShellTool;
use crate::tools::sub_agent::SpawnSubAgentTool;
use crate::tools::task::{CheckTaskTool, ListTasksTool, SpawnTaskTool};
use crate::tools::web_scrape::WebScrapeTool;
use crate::tools::web_search::WebSearchTool;
use crate::tools::youtube_transcript::YouTubeTranscriptTool;
use crate::tools::Tool;
use crate::workflow::{ToolInfo, WorkflowEngine};

#[derive(Parser)]
#[command(name = "zymi", about = "Zymi AI assistant", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Show debug output from all LLM calls
    #[arg(long, global = true)]
    debug: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Attach to agent interactively (debug / manual control)
    Cli,
    /// Stop the running daemon
    Stop,
    /// Check if daemon is running
    Status,
    /// Follow log output
    Logs,
    /// Run setup wizard
    Setup,
    /// Run evaluation suite
    Eval {
        /// Agent name
        agent: Option<String>,
        /// Run only specific eval by ID
        #[arg(long)]
        id: Option<String>,
        /// Number of runs for stability testing
        #[arg(long, default_value = "1")]
        runs: u32,
    },
    /// Update to latest release from GitHub
    Update,
    /// Log in with OpenAI (ChatGPT Plus/Pro) via OAuth
    Login {
        /// Use remote login flow (no local browser needed, for headless servers)
        #[arg(long)]
        remote: bool,
    },
    /// Log out from OpenAI OAuth
    Logout,
    /// Set up git backup for the memory directory
    Git,
    /// Run daemon in foreground (used internally by daemonizer)
    #[command(hide = true)]
    Run,
}

// ---------------------------------------------------------------------------
// Lightweight commands (no agent init needed)
// ---------------------------------------------------------------------------

/// Handle subcommands that don't need full initialization.
/// Returns `true` if a command was handled and main should exit.
async fn handle_lightweight_commands(cmd: &Option<Command>) -> bool {
    match cmd {
        Some(Command::Stop) => daemon::stop(),
        Some(Command::Status) => daemon::status(),
        Some(Command::Logs) => daemon::logs(),
        Some(Command::Update) => daemon::update().await,
        _ => return false,
    }
    true
}

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

fn init_logging(command: &Option<Command>, memory_dir: &Path) -> Result<()> {
    let log_format = |buf: &mut env_logger::fmt::Formatter, record: &log::Record| {
        use std::io::Write;
        writeln!(
            buf,
            "{} [{}] [{}] {}",
            buf.timestamp_millis(),
            record.level(),
            record.module_path().unwrap_or("-"),
            record.args()
        )
    };

    match command {
        Some(Command::Setup) => {
            std::env::set_var("RUST_LOG", "off");
            env_logger::Builder::from_default_env()
                .format(log_format)
                .init();
        }
        Some(Command::Cli) => {
            // CLI/TUI mode: log to file so TUI isn't broken
            if std::env::var("RUST_LOG").is_err() {
                std::env::set_var("RUST_LOG", "info");
            }
            let log_path = memory_dir.join("zymi.log");
            let log_file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .with_context(|| format!("Failed to open log file: {}", log_path.display()))?;
            env_logger::Builder::from_default_env()
                .format(log_format)
                .target(env_logger::Target::Pipe(Box::new(log_file)))
                .init();
        }
        Some(Command::Run) => {
            // Daemon foreground: log to stderr (daemon.rs redirects to zymi.log)
            if std::env::var("RUST_LOG").is_err() {
                std::env::set_var("RUST_LOG", "info");
            }
            env_logger::Builder::from_default_env()
                .format(log_format)
                .init();
        }
        _ => {
            env_logger::Builder::from_default_env()
                .format(log_format)
                .init();
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Eval mode
// ---------------------------------------------------------------------------

async fn run_eval_mode(
    memory_dir: &Path,
    agent: &Option<String>,
    filter_id: Option<&str>,
    runs: u32,
) -> Result<()> {
    let models_config = config::load_models_config(memory_dir);
    let provider: Arc<dyn LlmProvider> = Arc::new(
        ProviderManager::new(models_config, memory_dir.to_path_buf())
            .context("Failed to initialize provider manager for evals")?,
    );

    let agent_names = if let Some(ref name) = agent {
        vec![name.clone()]
    } else {
        let names = eval::list_eval_files(memory_dir);
        if names.is_empty() {
            println!("No eval files found in {}/evals/", memory_dir.display());
            println!("Use generate_evals tool to create them first.");
            return Ok(());
        }
        names
    };

    let runs = runs.max(1);

    // Stability tracking for multi-run mode
    let mut stability: std::collections::HashMap<String, (u32, u32)> =
        std::collections::HashMap::new();

    for run in 1..=runs {
        if runs > 1 {
            println!("--- Run {}/{} ---\n", run, runs);
        }

        for name in &agent_names {
            let suite = match eval::load_eval_suite(memory_dir, name) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("Error loading evals for '{}': {}", name, e);
                    continue;
                }
            };

            let report =
                eval::run_eval_suite(provider.clone(), memory_dir, &suite, filter_id).await;

            if let Err(e) = eval::save_eval_report(memory_dir, &report).await {
                log::warn!("Failed to save eval report: {e}");
            }

            print!("{}", eval::format_report(&report));

            // Track stability
            if runs > 1 {
                for result in &report.results {
                    let entry = stability
                        .entry(result.eval_id.clone())
                        .or_insert((0, 0));
                    if result.passed {
                        entry.0 += 1;
                    }
                    entry.1 += 1;
                }
            }
        }
    }

    if runs > 1 {
        print!("{}", eval::format_stability_report(runs, &stability));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// App builder
// ---------------------------------------------------------------------------

struct App {
    agent: Arc<Agent>,
    provider_manager: Arc<ProviderManager>,
    shared_approval_handler: SharedApprovalHandler,
    debug_rx: mpsc::UnboundedReceiver<DebugEvent>,
    models_config: ModelsConfig,
    system_prompt: String,
    provider: Arc<dyn LlmProvider>,
    memory_dir: PathBuf,
    git_sync: Arc<GitSync>,
    mcp_manager: Arc<tokio::sync::Mutex<McpManager>>,
    shutdown: tokio_util::sync::CancellationToken,
    langfuse: Option<Arc<LangfuseClient>>,
    ask_user_rx: mpsc::UnboundedReceiver<tools::ask_user::UserQuestion>,
    policy_engine: Arc<policy::PolicyEngine>,
    audit_log: audit::AuditLog,
    sandbox: Arc<sandbox::SandboxManager>,
    event_bus: Arc<events::bus::EventBus>,
}

async fn build_app(cli: &Cli, memory_dir: PathBuf) -> Result<App> {
    log::info!("Starting bot...");

    let git_sync = Arc::new(GitSync::new(&memory_dir));
    if git_sync.is_enabled() {
        git_sync.pull();
    }

    let db_path = memory_dir.join("conversations.db");
    let storage = Arc::new(
        SqliteStorage::new(&db_path)
            .with_context(|| format!("Failed to open SQLite database: {}", db_path.display()))?,
    );

    // Event store + bus (EDA foundation)
    let event_store = Arc::new(
        events::store::SqliteEventStore::new(&db_path)
            .with_context(|| "Failed to initialize event store")?,
    );
    let event_bus = Arc::new(events::bus::EventBus::new(event_store));

    let agent_prompt_path = memory_dir.join("AGENT.md");
    let system_prompt = std::fs::read_to_string(&agent_prompt_path).unwrap_or_else(|e| {
        log::warn!(
            "Failed to read {}: {e}, using default prompt",
            agent_prompt_path.display()
        );
        "You are a helpful assistant. You can use tools when needed to answer user questions. \
         Respond in the same language the user writes in."
            .to_string()
    });

    let models_config = config::load_models_config(&memory_dir);
    let provider_manager = Arc::new(
        ProviderManager::new(models_config.clone(), memory_dir.clone())
            .context("Failed to initialize provider manager")?,
    );

    let (debug_tx, debug_rx) = mpsc::unbounded_channel();
    let provider: Arc<dyn LlmProvider> = if cli.debug {
        Arc::new(DebugProvider::new(provider_manager.clone(), debug_tx))
    } else {
        drop(debug_tx);
        provider_manager.clone()
    };

    let shared_approval_handler = new_shared_approval_handler();
    let task_registry = task_registry::new_task_registry();

    // Policy engine for shell commands
    let policy_config = policy::load_policy(&memory_dir);
    let policy_engine = Arc::new(policy::PolicyEngine::new(policy_config));

    // Sandbox for shell command isolation
    let sandbox_config = sandbox::load_sandbox_config(&memory_dir);
    let sandbox_manager = Arc::new(sandbox::SandboxManager::new(sandbox_config).await);

    let (ask_user_tx, ask_user_rx) = mpsc::unbounded_channel();

    let mut tools: Vec<Box<dyn Tool>> = vec![
        Box::new(AskUserTool::new(ask_user_tx)),
        Box::new(PlanningTool::new(provider.clone(), memory_dir.clone())),
        Box::new(CurrentTimeTool),
        Box::new(ReadMemoryTool::new(memory_dir.clone())),
        Box::new(WriteMemoryTool::new(memory_dir.clone()).with_git_sync(git_sync.clone())),
        Box::new(
            ShellTool::new()
                .with_task_registry(task_registry.clone())
                .with_policy(policy_engine.clone())
                .with_sandbox(sandbox_manager.clone(), sandbox::ExecutionContext::Interactive),
        ),
        Box::new(RunCodeTool::new().with_sandbox(sandbox_manager.clone())),
        Box::new(
            SpawnSubAgentTool::new(
                provider.clone(),
                memory_dir.clone(),
                shared_approval_handler.clone(),
            )
            .with_sandbox(sandbox_manager.clone()),
        ),
        Box::new(CreateSubAgentTool::new(memory_dir.clone())),
        Box::new(
            SpawnTaskTool::new(
                provider.clone(),
                memory_dir.clone(),
                shared_approval_handler.clone(),
                task_registry.clone(),
            )
            .with_sandbox(sandbox_manager.clone()),
        ),
        Box::new(CheckTaskTool::new(task_registry.clone())),
        Box::new(ListTasksTool::new(task_registry.clone())),
        Box::new(ManageScheduleTool::new(memory_dir.clone())),
        Box::new(ManageMcpTool::new(memory_dir.clone())),
        Box::new(ManagePolicyTool::new(memory_dir.clone())),
        Box::new(GenerateEvalsTool::new(
            provider.clone(),
            memory_dir.clone(),
        )),
        Box::new(RunEvalsTool::new(provider.clone(), memory_dir.clone())),
    ];

    // Optional tools gated by API keys
    let optional_tools: Vec<(&str, Option<Box<dyn Tool>>)> = vec![
        ("web_search (TAVILY_API_KEY)", WebSearchTool::new().map(|t| Box::new(t) as _)),
        ("web_scrape (FIRECRAWL_API_KEY)", WebScrapeTool::new().map(|t| Box::new(t) as _)),
        ("youtube_transcript (SUPADATA_API_KEY)", YouTubeTranscriptTool::new().map(|t| Box::new(t) as _)),
    ];
    for (label, tool) in optional_tools {
        if let Some(tool) = tool {
            let name = label.split(' ').next().unwrap_or(label);
            log::info!("{name} tool enabled");
            tools.push(tool);
        } else {
            log::warn!("{label} not set, tool disabled");
        }
    }

    let (mcp_manager, mcp_tools) = McpManager::init(&memory_dir).await;
    tools.extend(mcp_tools);
    let mcp_manager = Arc::new(tokio::sync::Mutex::new(mcp_manager));

    // Collect tool catalog for the workflow planner (names + descriptions)
    let available_tools: Vec<ToolInfo> = tools
        .iter()
        .map(|t| {
            let def = t.definition();
            ToolInfo {
                name: def.name,
                description: def.description,
            }
        })
        .collect();

    let workflow_engine = Arc::new(
        WorkflowEngine::new(
            provider.clone(),
            memory_dir.clone(),
            available_tools,
            shared_approval_handler.clone(),
        )
        .with_sandbox(sandbox_manager.clone()),
    );

    let langfuse = LangfuseConfig::from_env().map(LangfuseClient::new);
    let default_model_id = models_config
        .models
        .iter()
        .find(|m| m.is_default)
        .or_else(|| models_config.models.first())
        .map(|m| m.id.clone())
        .unwrap_or_default();

    let audit_log = audit::AuditLog::new(&memory_dir);

    let settings = &models_config.settings;

    // Tool selection (RAG for tools): embed tool descriptions, select top-K per query
    let tool_selector = if settings.tool_selection {
        let openai_key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
        if !openai_key.is_empty() {
            let all_defs: Vec<_> = tools.iter().map(|t| t.definition()).collect();
            let always_available: std::collections::HashSet<String> = [
                "think", "ask_user", "get_current_time", "read_memory",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();

            let mut selector = ToolSelector::new(
                openai_key,
                Some(memory_dir.join("tool_embeddings.json")),
                always_available,
            )
            .with_top_k(settings.tool_selection_top_k);

            match selector.initialize(&all_defs).await {
                Ok(_) => {
                    log::info!("Tool selector initialized: {} tools embedded", all_defs.len());
                    Some(selector)
                }
                Err(e) => {
                    log::warn!("Tool selector init failed: {e}, using all tools");
                    None
                }
            }
        } else {
            log::info!("No OpenAI key for embeddings, tool selection disabled");
            None
        }
    } else {
        None
    };

    let mut agent_builder = Agent::new(provider.clone(), tools, Some(system_prompt.clone()), storage)
        .with_max_iterations(settings.max_iterations)
        .with_summary_threshold(settings.summary_threshold)
        .with_monitor(MonitorConfig::new(provider.clone()))
        .with_memory_dir(memory_dir.clone())
        .with_workflow_engine(workflow_engine)
        .with_audit(audit_log.clone())
        .with_auto_extract(settings.auto_extract)
        .with_mcp_manager(mcp_manager.clone());

    if let Some(selector) = tool_selector {
        agent_builder = agent_builder.with_tool_selector(selector);
    }

    if let Some(ref lf) = langfuse {
        agent_builder = agent_builder.with_langfuse(lf.clone(), default_model_id);
    }

    let agent = Arc::new(agent_builder);

    let shutdown = tokio_util::sync::CancellationToken::new();

    Ok(App {
        agent,
        provider_manager,
        shared_approval_handler,
        debug_rx,
        models_config,
        system_prompt,
        provider,
        memory_dir,
        git_sync,
        mcp_manager,
        shutdown,
        langfuse,
        ask_user_rx,
        policy_engine,
        audit_log,
        sandbox: sandbox_manager,
        event_bus,
    })
}

// ---------------------------------------------------------------------------
// Shutdown
// ---------------------------------------------------------------------------

async fn shutdown_app(
    app_shutdown: tokio_util::sync::CancellationToken,
    git_sync: Arc<GitSync>,
    mcp_manager: Arc<tokio::sync::Mutex<McpManager>>,
    bg_handles: Vec<JoinHandle<()>>,
) {
    app_shutdown.cancel();

    // Wait for background tasks with timeout
    for handle in bg_handles {
        if tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .is_err()
        {
            log::warn!("Background task did not finish within 5s");
        }
    }

    // Final git push
    if git_sync.is_enabled() {
        git_sync.commit("shutdown");
        git_sync.push();
    }

    // MCP shutdown with timeout
    match Arc::try_unwrap(mcp_manager) {
        Ok(mutex) => {
            let manager = mutex.into_inner();
            if tokio::time::timeout(std::time::Duration::from_secs(5), manager.shutdown())
                .await
                .is_err()
            {
                log::warn!("MCP shutdown timed out after 5s");
            }
        }
        Err(_) => {
            log::warn!("MCP manager still has references, skipping graceful shutdown");
        }
    }
}

// ---------------------------------------------------------------------------
// Entrypoint
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    daemon::apply_systemd_env();
    let cli = Cli::parse();

    // Lightweight commands — no heavy init needed
    if handle_lightweight_commands(&cli.command).await {
        return Ok(());
    }

    // Memory dir (needed before logging and setup checks)
    let memory_dir: PathBuf = std::env::var("MEMORY_DIR")
        .unwrap_or_else(|_| "./memory".to_string())
        .into();
    std::fs::create_dir_all(&memory_dir)
        .with_context(|| format!("Failed to create memory directory: {}", memory_dir.display()))?;

    // Scaffold default files (AGENT.md, .gitignore, etc.) on first run
    setup::init_memory_dir(&memory_dir);

    // Default (no args) → start daemon in background
    if cli.command.is_none() {
        if setup::needs_setup(&memory_dir) && !setup::run_setup(&memory_dir) {
            return Ok(());
        }
        daemon::start();
        return Ok(());
    }

    init_logging(&cli.command, &memory_dir)?;

    // Setup wizard
    if let Some(Command::Setup) = &cli.command {
        setup::run_setup(&memory_dir);
        return Ok(());
    }

    // Git backup setup
    if let Some(Command::Git) = &cli.command {
        setup::run_git_setup(&memory_dir);
        return Ok(());
    }

    // OAuth login/logout
    if let Some(Command::Login { remote }) = &cli.command {
        if *remote {
            auth::login::login_remote(&memory_dir).await?;
        } else {
            auth::login::login(&memory_dir).await?;
        }
        daemon::sync_to_service(&memory_dir, &["auth.json", "models.json"]);
        return Ok(());
    }
    if let Some(Command::Logout) = &cli.command {
        auth::login::logout(&memory_dir);
        daemon::sync_to_service(&memory_dir, &["auth.json"]);
        return Ok(());
    }

    // Eval mode
    if let Some(Command::Eval { agent, id, runs }) = &cli.command {
        return run_eval_mode(&memory_dir, agent, id.as_deref(), *runs).await;
    }

    // First-run detection for interactive CLI
    if matches!(cli.command, Some(Command::Cli)) && setup::needs_setup(&memory_dir)
        && !setup::run_setup(&memory_dir) {
            return Ok(());
        }

    // Full agent initialization
    let app = build_app(&cli, memory_dir).await?;

    // Spawn background tasks, keep handles for graceful shutdown
    let mut bg_handles: Vec<JoinHandle<()>> = Vec::new();

    bg_handles.push(tokio::spawn(git_sync::heartbeat(
        app.memory_dir.clone(),
        app.git_sync.clone(),
        app.shutdown.clone(),
        app.models_config.settings.heartbeat_interval_secs,
    )));

    if let Some(ref lf) = app.langfuse {
        bg_handles.push(lf.start_flushing(app.shutdown.clone()));
    }

    // Agent worker: consumes inbound events, drives agent, publishes responses
    let agent_worker = Arc::new(events::agent_worker::AgentWorker::new(
        app.agent.clone(),
        app.event_bus.clone(),
        app.shared_approval_handler.clone(),
    ));
    bg_handles.push(tokio::spawn(async move { agent_worker.run().await }));

    // Destructure what we need before moving into connector
    let shutdown = app.shutdown.clone();
    let git_sync = app.git_sync.clone();
    let mcp_manager = app.mcp_manager;

    match cli.command {
        Some(Command::Cli) => {
            connectors::cli::run(
                app.agent,
                app.provider_manager,
                app.shared_approval_handler,
                app.debug_rx,
                &app.models_config,
                app.ask_user_rx,
            )
            .await;
        }
        Some(Command::Run) => {
            let has_telegram = std::env::var("TELOXIDE_TOKEN")
                .map(|v| !v.is_empty())
                .unwrap_or(false)
                && std::env::var("ALLOWED_USERS")
                    .map(|v| !v.is_empty())
                    .unwrap_or(false);

            let default_model = app.models_config.models.iter()
                .find(|m| m.is_default)
                .or_else(|| app.models_config.models.first())
                .map(|m| m.id.clone())
                .unwrap_or_else(|| "unknown".to_string());
            app.audit_log.log(audit::AuditEvent::AgentStart {
                model: default_model,
            });

            bg_handles.push(tokio::spawn(scheduler::run(
                app.provider.clone(),
                app.memory_dir.clone(),
                app.system_prompt,
                app.policy_engine.clone(),
                app.audit_log.clone(),
                shutdown.clone(),
                Some(app.sandbox.clone()),
            )));

            if has_telegram {
                log::info!("Telegram connector enabled");
                connectors::telegram::run(
                    app.agent,
                    app.provider_manager,
                    app.shared_approval_handler,
                    app.ask_user_rx,
                )
                .await;
            } else {
                log::info!("Telegram not configured, running background tasks only");
                // Wait for termination signal (Ctrl+C or SIGTERM from `zymi stop`)
                #[cfg(unix)]
                {
                    let mut sigterm = tokio::signal::unix::signal(
                        tokio::signal::unix::SignalKind::terminate(),
                    )
                    .expect("Failed to register SIGTERM handler");
                    tokio::select! {
                        _ = tokio::signal::ctrl_c() => {}
                        _ = sigterm.recv() => {}
                    }
                }
                #[cfg(not(unix))]
                {
                    tokio::signal::ctrl_c().await.ok();
                }
            }

            app.audit_log.log(audit::AuditEvent::AgentStop);
        }
        _ => unreachable!(),
    }

    shutdown_app(shutdown, git_sync, mcp_manager, bg_handles).await;

    Ok(())
}
