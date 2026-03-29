mod app;
mod approval;
mod input;
mod markdown;
mod theme;
mod ui;

use std::io::{self, Write};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::EventStream;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::execute;
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::core::approval::{ApprovalSlotGuard, SharedApprovalHandler};
use crate::core::config::{ModelEntry, ModelsConfig, ProviderType};
use crate::core::debug_provider::DebugEvent;
use crate::core::provider_manager::ProviderManager;
use crate::core::StreamEvent;
use crate::events::bus::EventBus;
use crate::events::connector::EventDrivenConnector;
use crate::setup;

use app::App;
use approval::{AppEvent, CliApprovalHandler};
use input::{handle_event, InputAction};

/// Enable mouse tracking for button clicks and scroll only (no movement tracking).
/// Avoids \x1b[?1003h (any-event/move tracking) which floods SSH connections
/// with SGR escape sequences that can leak into text input.
fn enable_mouse_scroll() {
    let mut stdout = io::stdout();
    // ?1000h = normal button tracking (clicks + scroll)
    // ?1002h = button-event tracking (drag)
    // ?1006h = SGR extended mouse mode (for coordinates > 223)
    // Intentionally omit ?1003h (all-movements tracking)
    let _ = stdout.write_all(b"\x1b[?1000h\x1b[?1002h\x1b[?1006h");
    let _ = stdout.flush();
}

/// Disable mouse tracking (mirrors enable_mouse_scroll).
fn disable_mouse_scroll() {
    let mut stdout = io::stdout();
    let _ = stdout.write_all(b"\x1b[?1006l\x1b[?1002l\x1b[?1000l");
    let _ = stdout.flush();
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    connector: Arc<EventDrivenConnector>,
    event_bus: Arc<EventBus>,
    provider_manager: Arc<ProviderManager>,
    shared_approval: SharedApprovalHandler,
    mut debug_rx: mpsc::UnboundedReceiver<DebugEvent>,
    models_config: &ModelsConfig,
    mut ask_user_rx: mpsc::UnboundedReceiver<crate::tools::ask_user::UserQuestion>,
    memory_dir: std::path::PathBuf,
) {
    // Setup terminal
    enable_raw_mode().expect("Failed to enable raw mode");
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).expect("Failed to enter alternate screen");
    enable_mouse_scroll();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).expect("Failed to create terminal");

    // Set panic hook to restore terminal
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        disable_mouse_scroll();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(info);
    }));

    let conversation_id = "main".to_string();

    let available_models: Vec<app::ModelSelectorEntry> = provider_manager
        .available_models()
        .await
        .into_iter()
        .map(|m| app::ModelSelectorEntry {
            id: m.id,
            name: m.name,
        })
        .collect();
    let current_model_id = provider_manager.current_model_id().await;

    let debug_mode = !debug_rx.is_closed();

    // Look up pricing for the current model
    let current_entry = models_config
        .models
        .iter()
        .find(|m| m.id == current_model_id);
    let input_price = current_entry.and_then(|e| e.input_price_per_1m);
    let output_price = current_entry.and_then(|e| e.output_price_per_1m);

    let mut app = App::new(
        conversation_id,
        available_models,
        current_model_id,
        debug_mode,
        input_price,
        output_price,
        memory_dir,
    );

    // Channel for stream events from agent
    let (stream_tx, mut stream_rx) = mpsc::unbounded_channel::<StreamEvent>();

    // Channel for approval events
    let (approval_tx, mut approval_rx) = mpsc::unbounded_channel::<AppEvent>();

    // EventBus subscriber for right panel (observability)
    let mut domain_rx = event_bus.subscribe_with_capacity(1024).await;

    // Crossterm event stream
    let mut reader = EventStream::new();

    // Tick interval for spinner
    let mut tick_interval = tokio::time::interval(Duration::from_millis(100));

    // Track current agent task so we can abort on Esc
    let mut agent_task: Option<JoinHandle<()>> = None;

    loop {
        // Draw UI
        terminal
            .draw(|f| ui::draw(f, &mut app))
            .expect("Failed to draw");

        if app.should_quit {
            break;
        }

        tokio::select! {
            // Keyboard events
            maybe_event = reader.next() => {
                if let Some(Ok(event)) = maybe_event {
                    match handle_event(&mut app, event) {
                        InputAction::SendMessage(text) => {
                            app.messages.push(app::ChatEntry::UserMessage(text.clone()));
                            app.is_processing = true;
                            app.scroll_to_bottom();

                            let connector = connector.clone();
                            let conv_id = app.conversation_id.clone();
                            let tx = stream_tx.clone();
                            let approval_tx = approval_tx.clone();
                            let shared_approval = shared_approval.clone();

                            agent_task = Some(tokio::spawn(async move {
                                let approval_handler: Arc<dyn crate::core::approval::ApprovalHandler> =
                                    Arc::new(CliApprovalHandler::new(approval_tx));

                                let _guard = ApprovalSlotGuard::set(
                                    shared_approval,
                                    approval_handler,
                                )
                                .await;

                                let result = connector
                                    .submit_and_wait_streaming(
                                        &conv_id,
                                        crate::core::Message::User(text),
                                        "cli",
                                        Duration::from_secs(600),
                                        tx.clone(),
                                    )
                                    .await;

                                if let Err(e) = result {
                                    let _ = tx.send(StreamEvent::Error(e.to_string()));
                                }
                            }));
                        }
                        InputAction::Interrupt => {
                            if let Some(handle) = agent_task.take() {
                                handle.abort();
                            }
                            app.is_processing = false;
                            app.pending_approval = None;
                            app.messages.push(app::ChatEntry::SystemMessage(
                                "Interrupted.".to_string(),
                            ));
                            if app.auto_scroll {
                                app.scroll_to_bottom();
                            }
                        }
                        InputAction::AddModel(info) => {
                            let provider = match info.provider_index {
                                0 => ProviderType::OpenaiCompatible,
                                1 => ProviderType::Anthropic,
                                2 => ProviderType::ChatgptOauth,
                                _ => ProviderType::OpenaiCompatible,
                            };

                            // ChatGPT OAuth: launch login flow if no tokens
                            if provider == ProviderType::ChatgptOauth {
                                let memory_dir = provider_manager.memory_dir().to_path_buf();
                                let has_tokens = crate::auth::storage::load_tokens(&memory_dir).is_some();

                                if !has_tokens {
                                    app.messages.push(app::ChatEntry::SystemMessage(
                                        "Opening browser for OpenAI authentication...".to_string(),
                                    ));

                                    let tx = stream_tx.clone();

                                    tokio::spawn(async move {
                                        match crate::auth::login::login(&memory_dir).await {
                                            Ok(()) => {
                                                // login() already fetches models and saves them
                                                let _ = tx.send(StreamEvent::Done(
                                                    "OAuth login successful! Models configured. Switch with Ctrl+M.".to_string(),
                                                ));
                                            }
                                            Err(e) => {
                                                let _ = tx.send(StreamEvent::Error(
                                                    format!("OAuth login failed: {e}"),
                                                ));
                                            }
                                        }
                                    });
                                    continue;
                                }

                                // Tokens exist — models were configured during login
                                app.messages.push(app::ChatEntry::SystemMessage(
                                    "Already logged in. ChatGPT models available via Ctrl+M.".to_string(),
                                ));
                                continue;
                            }

                            let base_url = if info.base_url.is_empty() {
                                None
                            } else {
                                Some(info.base_url)
                            };

                            // Save API key to .env and reference by env var name
                            let api_key_env = if !info.api_key.is_empty() && !info.env_var_name.is_empty() {
                                setup::append_env_var(&info.env_var_name, &info.api_key);
                                std::env::set_var(&info.env_var_name, &info.api_key);
                                info.env_var_name.clone()
                            } else {
                                String::new()
                            };

                            let entry = ModelEntry {
                                id: info.model_id.clone(),
                                name: info.display_name.clone(),
                                provider,
                                api_key_env,
                                base_url,
                                api_key: None,
                                is_default: false,
                                input_price_per_1m: None,
                                output_price_per_1m: None,
                            };

                            match provider_manager.add_model(entry).await {
                                Ok(()) => {
                                    app.available_models.push(app::ModelSelectorEntry {
                                        id: info.model_id.clone(),
                                        name: info.display_name.clone(),
                                    });
                                    app.messages.push(app::ChatEntry::SystemMessage(
                                        format!("Added model: {}", info.display_name),
                                    ));
                                }
                                Err(e) => {
                                    app.messages.push(app::ChatEntry::SystemMessage(
                                        format!("Failed to add model: {}", e),
                                    ));
                                }
                            }
                        }
                        InputAction::SwitchModel(model_id) => {
                            let pm = provider_manager.clone();
                            let mid = model_id.clone();
                            match pm.switch_model(&mid).await {
                                Ok(()) => {
                                    app.current_model_id = model_id;
                                    // Update pricing for new model
                                    let new_entry = models_config
                                        .models
                                        .iter()
                                        .find(|m| m.id == app.current_model_id);
                                    app.input_price_per_1m = new_entry.and_then(|e| e.input_price_per_1m);
                                    app.output_price_per_1m = new_entry.and_then(|e| e.output_price_per_1m);
                                    app.messages.push(app::ChatEntry::SystemMessage(
                                        format!("Switched to model: {}", app.current_model_id),
                                    ));
                                }
                                Err(e) => {
                                    app.messages.push(app::ChatEntry::SystemMessage(
                                        format!("Failed to switch model: {}", e),
                                    ));
                                }
                            }
                        }
                        InputAction::OpenEditor(path) => {
                            // Leave alternate screen for external editor
                            disable_mouse_scroll();
                            let _ = disable_raw_mode();
                            let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);

                            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".to_string());
                            let status = std::process::Command::new(&editor)
                                .arg(&path)
                                .status();

                            // Re-enter alternate screen
                            let _ = enable_raw_mode();
                            let _ = execute!(terminal.backend_mut(), EnterAlternateScreen);
                            enable_mouse_scroll();
                            terminal.clear().ok();

                            if let Err(e) = status {
                                app.messages.push(app::ChatEntry::SystemMessage(
                                    format!("Failed to open editor: {e}"),
                                ));
                            }

                            // Rescan subagents in case user created/edited one
                            let subagent_dir = app.memory_dir.join("subagents");
                            app.subagent_files = std::fs::read_dir(&subagent_dir)
                                .ok()
                                .map(|entries| {
                                    entries
                                        .filter_map(|e| e.ok())
                                        .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
                                        .filter_map(|e| e.file_name().into_string().ok())
                                        .collect()
                                })
                                .unwrap_or_default();
                        }
                        InputAction::ToggleCopyMode => {
                            app.copy_mode = !app.copy_mode;
                            if app.copy_mode {
                                disable_mouse_scroll();
                            } else {
                                enable_mouse_scroll();
                            }
                        }
                        InputAction::Quit => {
                            app.should_quit = true;
                        }
                        InputAction::None => {}
                    }
                }
            }
            // Stream events from agent
            Some(event) = stream_rx.recv() => {
                // Refresh model list on Done (may have been added by background OAuth)
                if matches!(event, StreamEvent::Done(_)) {
                    let models = provider_manager.available_models().await;
                    app.available_models = models
                        .into_iter()
                        .map(|m| app::ModelSelectorEntry { id: m.id, name: m.name })
                        .collect();
                }
                app.handle_stream_event(event);
            }
            // Approval events
            Some(event) = approval_rx.recv() => {
                match event {
                    AppEvent::ApprovalRequest(pending) => {
                        app.pending_approval = Some(pending);
                        app.approval_selected = true; // default to Yes
                        if app.auto_scroll {
                            app.scroll_to_bottom();
                        }
                    }
                }
            }
            // ask_user tool events (routed through same mechanism)
            Some(q) = ask_user_rx.recv() => {
                app.messages.push(app::ChatEntry::SystemMessage(
                    format!("❓ {}", q.question),
                ));
                app.pending_question = Some(app::PendingQuestion {
                    responder: q.responder,
                });
                app.scroll_to_bottom();
            }
            // Domain events for right panel (observability)
            Some(event) = domain_rx.recv() => {
                app.handle_domain_event(event);
            }
            // Debug events
            Some(event) = debug_rx.recv() => {
                if app.debug_mode {
                    app.handle_debug_event(event);
                }
            }
            // Spinner tick
            _ = tick_interval.tick() => {
                if app.is_processing {
                    app.tick_spinner();
                }
            }
        }
    }

    // Restore terminal
    disable_mouse_scroll();
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
}
