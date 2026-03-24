use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::{
    InlineKeyboardButton, InlineKeyboardMarkup, MaybeInaccessibleMessage, ParseMode, UpdateKind,
};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::core::agent::Agent;
use crate::core::approval::{ApprovalHandler, ApprovalSlotGuard, SharedApprovalHandler};
use crate::core::provider_manager::ProviderManager;
use crate::core::transcription::TranscriptionService;
use crate::core::{ContentPart, LlmError};
use crate::tools::ask_user::UserQuestion;

const TELEGRAM_MSG_LIMIT: usize = 4096;
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);
/// Timeout for Telegram API calls (send_message, send_chat_action, etc.).
/// Prevents hung TCP connections from blocking the handler (and all queued messages)
/// indefinitely due to teloxide's per-chat serialization.
const API_TIMEOUT: Duration = Duration::from_secs(30);

pub fn bot_with_timeout() -> Bot {
    let client = reqwest::Client::builder()
        .timeout(API_TIMEOUT)
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("failed to build reqwest client");
    Bot::with_client(
        std::env::var("TELOXIDE_TOKEN")
            .expect("TELOXIDE_TOKEN must be set"),
        client,
    )
}

/// Escape special HTML characters for Telegram HTML parse mode.
pub fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Split text into chunks that fit within Telegram's message size limit.
/// Tries to break on newlines first, then on spaces, falling back to hard cuts.
fn split_message(text: &str) -> Vec<&str> {
    if text.len() <= TELEGRAM_MSG_LIMIT {
        return vec![text];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        if remaining.len() <= TELEGRAM_MSG_LIMIT {
            chunks.push(remaining);
            break;
        }

        let boundary = remaining.floor_char_boundary(TELEGRAM_MSG_LIMIT);
        let split_at = remaining[..boundary]
            .rfind('\n')
            .or_else(|| remaining[..boundary].rfind(' '))
            .map(|pos| pos + 1)
            .unwrap_or(boundary);

        chunks.push(&remaining[..split_at]);
        remaining = &remaining[split_at..];
    }

    chunks
}

async fn send_long_message(bot: &Bot, chat_id: ChatId, text: &str) -> ResponseResult<()> {
    for chunk in split_message(text) {
        bot.send_message(chat_id, chunk).await?;
    }
    Ok(())
}

type PendingApprovals = Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>;
type PendingQuestions = Arc<std::sync::Mutex<HashMap<ChatId, oneshot::Sender<String>>>>;
type ActiveChat = Arc<std::sync::Mutex<Option<ChatId>>>;
type SharedTranscription = Arc<Option<TranscriptionService>>;

/// Global pending approvals state, used by `TelegramApprovalHandler`.
static PENDING_APPROVALS: std::sync::OnceLock<PendingApprovals> = std::sync::OnceLock::new();

/// Global pending questions state, accessible from the distribution function
/// (which takes a `fn` pointer and cannot capture state).
static PENDING_QUESTIONS: std::sync::OnceLock<PendingQuestions> = std::sync::OnceLock::new();

/// Distribution function for the teloxide dispatcher.
/// Callbacks and text replies to ask_user bypass per-chat serialization to avoid
/// deadlocks. Non-text messages (photos, voice) cancel the pending question and
/// queue behind the current handler so we don't get two concurrent agent processes
/// corrupting the same conversation history.
fn distribute_update(update: &Update) -> Option<ChatId> {
    match &update.kind {
        UpdateKind::CallbackQuery(_) => None,
        UpdateKind::Message(msg) => {
            let chat_id = msg.chat.id;
            if let Some(pq) = PENDING_QUESTIONS.get() {
                if let Ok(mut q) = pq.lock() {
                    if q.contains_key(&chat_id) {
                        if msg.text().is_some() {
                            // Text reply → bypass serialization so it reaches
                            // the pending ask_user immediately.
                            return None;
                        }
                        // Non-text (photo/voice/etc.) → cancel the pending
                        // question to unblock the first handler, but DON'T
                        // bypass serialization: this message will queue and
                        // run only after the first handler finishes.
                        if let Some(responder) = q.remove(&chat_id) {
                            let _ = responder.send(
                                "[MEDIA_RECEIVED] User sent a photo/media in response. \
                                 Stop now — it will be processed as the next message."
                                    .to_string(),
                            );
                        }
                        return Some(chat_id);
                    }
                }
            }
            Some(chat_id)
        }
        _ => update.chat().map(|c| c.id),
    }
}

pub fn allowed_users() -> Result<Vec<UserId>, String> {
    let val = std::env::var("ALLOWED_USERS")
        .map_err(|_| "ALLOWED_USERS env var is not set".to_string())?;
    let mut users = Vec::new();
    for s in val.split(',') {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            continue;
        }
        let id: u64 = trimmed
            .parse()
            .map_err(|_| format!("ALLOWED_USERS: invalid user ID '{trimmed}'"))?;
        users.push(UserId(id));
    }
    if users.is_empty() {
        return Err("ALLOWED_USERS is empty".to_string());
    }
    Ok(users)
}

pub struct TelegramApprovalHandler {
    bot: Bot,
    chat_id: ChatId,
    pending: PendingApprovals,
}

impl TelegramApprovalHandler {
    pub fn new(bot: Bot, chat_id: ChatId, pending: PendingApprovals) -> Self {
        Self {
            bot,
            chat_id,
            pending,
        }
    }
}

/// Drop guard that ensures the pending approval entry is removed from the HashMap
/// even if the future is cancelled or panics.
struct ApprovalGuard {
    approval_id: Option<String>,
    pending: PendingApprovals,
}

impl ApprovalGuard {
    fn new(approval_id: String, pending: PendingApprovals) -> Self {
        Self {
            approval_id: Some(approval_id),
            pending,
        }
    }

    /// Disarm the guard — the entry was already consumed by callback_handler.
    fn disarm(&mut self) {
        self.approval_id = None;
    }
}

impl Drop for ApprovalGuard {
    fn drop(&mut self) {
        if let Some(id) = self.approval_id.take() {
            // try_lock avoids blocking in Drop. If contended, the entry
            // will be a harmless stale key — callback_handler already
            // handles missing entries gracefully ("Запрос устарел").
            if let Ok(mut map) = self.pending.try_lock() {
                map.remove(&id);
            } else {
                log::warn!("ApprovalGuard: could not clean up entry {id} (mutex contended)");
            }
        }
    }
}

#[async_trait]
impl ApprovalHandler for TelegramApprovalHandler {
    async fn request_approval(
        &self,
        tool_description: &str,
        explanation: Option<&str>,
    ) -> Result<bool, String> {
        let approval_id = uuid::Uuid::new_v4().to_string();

        let mut text = format!("🔐 <b>Approval required</b>\n\n{}", escape_html(tool_description));
        if let Some(expl) = explanation {
            text.push_str(&format!("\n\n<b>Reason:</b> {}", escape_html(expl)));
        }

        let keyboard = InlineKeyboardMarkup::new(vec![vec![
            InlineKeyboardButton::callback("✅ Approve", format!("approve:{approval_id}")),
            InlineKeyboardButton::callback("❌ Reject", format!("reject:{approval_id}")),
        ]]);

        let (tx, rx) = oneshot::channel();

        {
            let mut pending = self.pending.lock().await;
            pending.insert(approval_id.clone(), tx);
        }

        let mut guard = ApprovalGuard::new(approval_id, self.pending.clone());

        self.bot
            .send_message(self.chat_id, text)
            .parse_mode(ParseMode::Html)
            .reply_markup(keyboard)
            .await
            .map_err(|e| format!("Failed to send approval request: {e}"))?;

        let result = tokio::time::timeout(APPROVAL_TIMEOUT, rx).await;

        match result {
            Ok(Ok(approved)) => {
                // callback_handler already removed the entry
                guard.disarm();
                Ok(approved)
            }
            Ok(Err(_)) => {
                // Sender dropped (entry already gone from map)
                guard.disarm();
                Ok(false)
            }
            Err(_) => {
                // Timeout — guard will clean up on drop
                Ok(false)
            }
        }
    }
}

async fn callback_handler(
    bot: Bot,
    q: CallbackQuery,
    pending: PendingApprovals,
) -> ResponseResult<()> {
    let data = match q.data {
        Some(ref d) => d.clone(),
        None => return Ok(()),
    };

    // ---- Tool approval callbacks (approve / reject) ----
    let (action, approval_id) = match data.split_once(':') {
        Some((action, id)) => (action, id),
        None => return Ok(()),
    };

    let approved = match action {
        "approve" => true,
        "reject" => false,
        _ => return Ok(()),
    };

    let sender = {
        let mut pending = pending.lock().await;
        pending.remove(approval_id)
    };

    let status_label = if let Some(tx) = sender {
        log::info!(
            "Approval callback: id={approval_id}, decision={}",
            if approved { "approved" } else { "rejected" }
        );
        let _ = tx.send(approved);
        if approved { "✅ Approved" } else { "❌ Rejected" }
    } else {
        bot.answer_callback_query(q.id.clone())
            .text("⏳ Request expired")
            .await
            .ok();
        "⏳ Expired"
    };

    // Edit the original message to show the result and remove keyboard
    if let Some(MaybeInaccessibleMessage::Regular(msg)) = q.message {
        let original_text = escape_html(msg.text().unwrap_or(""));
        let new_text = format!("{original_text}\n\n<b>Status: {status_label}</b>");

        bot.edit_message_text(msg.chat.id, msg.id, new_text)
            .parse_mode(ParseMode::Html)
            .await
            .ok();

        bot.edit_message_reply_markup(msg.chat.id, msg.id)
            .await
            .ok();
    }

    bot.answer_callback_query(q.id).await.ok();

    Ok(())
}

pub async fn run(
    agent: Arc<Agent>,
    provider_manager: Arc<ProviderManager>,
    shared_approval: SharedApprovalHandler,
    ask_user_rx: mpsc::UnboundedReceiver<UserQuestion>,
) {
    let bot = bot_with_timeout();
    let allowed_users = match allowed_users() {
        Ok(users) => users,
        Err(e) => {
            log::error!("Failed to start Telegram bot: {e}");
            eprintln!("Error: {e}");
            return;
        }
    };
    let pending: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
    PENDING_APPROVALS.set(pending.clone()).ok();
    let pending_questions: PendingQuestions = Arc::new(std::sync::Mutex::new(HashMap::new()));
    PENDING_QUESTIONS.set(pending_questions.clone()).ok();
    let active_chat: ActiveChat = Arc::new(std::sync::Mutex::new(None));
    let start_time = std::time::Instant::now();

    let transcription: SharedTranscription = Arc::new(
        std::env::var("OPENAI_API_KEY").ok().map(|key| {
            let base_url = std::env::var("OPENAI_BASE_URL").ok();
            log::info!("Transcription service initialized (Whisper)");
            TranscriptionService::new(&key, base_url.as_deref())
        }),
    );

    let pending_for_handler = pending.clone();

    // Forward ask_user questions to the active Telegram chat
    tokio::spawn(ask_user_forwarder(
        bot.clone(),
        ask_user_rx,
        active_chat.clone(),
        pending_questions.clone(),
    ));

    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .filter(move |msg: Message| match msg.from {
                    Some(ref user) => allowed_users.contains(&user.id),
                    None => false,
                })
                .endpoint(gpt_handler),
        )
        .branch(Update::filter_callback_query().endpoint(callback_handler));

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![
            agent,
            provider_manager,
            pending_for_handler,
            shared_approval,
            start_time,
            pending_questions.clone(),
            active_chat,
            transcription
        ])
        // Allow callback queries and ask_user replies to run concurrently.
        // Without this, ask_user deadlocks: gpt_handler waits for the user's
        // reply, but the reply is queued behind the running handler.
        .distribution_function(distribute_update)
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}

// ---------------------------------------------------------------------------
// Slash commands
// ---------------------------------------------------------------------------

enum SlashCommand {
    Model(Option<String>),
    Clear,
    Status,
    Help,
}

fn parse_slash_command(text: &str) -> Option<SlashCommand> {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return None;
    }

    // Strip @botname suffix (e.g. /model@mybot)
    let first_word = trimmed.split_whitespace().next().unwrap_or("");
    let command = first_word.split('@').next().unwrap_or(first_word);
    let args: &str = trimmed[first_word.len()..].trim();

    match command {
        "/model" | "/models" => {
            let model_id = if args.is_empty() { None } else { Some(args.to_string()) };
            Some(SlashCommand::Model(model_id))
        }
        "/clear" | "/reset" => Some(SlashCommand::Clear),
        "/status" => Some(SlashCommand::Status),
        "/help" | "/start" => Some(SlashCommand::Help),
        _ => None,
    }
}

async fn handle_model_command(
    bot: &Bot,
    chat_id: ChatId,
    provider_manager: &Arc<ProviderManager>,
    model_id: Option<String>,
) -> ResponseResult<()> {
    match model_id {
        None => {
            // List models
            let current = provider_manager.current_model_id().await;
            let models = provider_manager.available_models().await;

            let mut lines = vec!["<b>Available models:</b>".to_string()];
            for m in &models {
                let marker = if m.id == current { " ◀" } else { "" };
                lines.push(format!("  <code>{}</code> — {}{}", m.id, m.name, marker));
            }
            lines.push(String::new());
            lines.push("Switch: <code>/model &lt;id&gt;</code>".to_string());

            bot.send_message(chat_id, lines.join("\n"))
                .parse_mode(ParseMode::Html)
                .await?;
        }
        Some(id) => {
            match provider_manager.switch_model(&id).await {
                Ok(()) => {
                    bot.send_message(chat_id, format!("Switched to model: <code>{id}</code>"))
                        .parse_mode(ParseMode::Html)
                        .await?;
                }
                Err(e) => {
                    let models = provider_manager.available_models().await;
                    let names: Vec<String> = models.iter().map(|m| m.id.clone()).collect();
                    bot.send_message(
                        chat_id,
                        format!("Error: {e}\n\nAvailable: {}", names.join(", ")),
                    )
                    .await?;
                }
            }
        }
    }
    Ok(())
}

async fn handle_clear_command(
    bot: &Bot,
    chat_id: ChatId,
    agent: &Arc<Agent>,
) -> ResponseResult<()> {
    let conversation_id = format!("telegram:{}", chat_id.0);
    match agent.clear_history(&conversation_id).await {
        Ok(()) => {
            bot.send_message(chat_id, "Conversation history cleared.")
                .await?;
        }
        Err(e) => {
            bot.send_message(chat_id, format!("Error clearing history: {e}"))
                .await?;
        }
    }
    Ok(())
}

async fn handle_status_command(
    bot: &Bot,
    chat_id: ChatId,
    provider_manager: &Arc<ProviderManager>,
    start_time: std::time::Instant,
) -> ResponseResult<()> {
    let model = provider_manager.current_model_id().await;
    let uptime = start_time.elapsed();
    let hours = uptime.as_secs() / 3600;
    let minutes = (uptime.as_secs() % 3600) / 60;

    let text = format!(
        "<b>Zymi v{}</b>\n\n\
         Model: <code>{model}</code>\n\
         Uptime: {hours}h {minutes}m\n\
         Chat: <code>telegram:{}</code>",
        env!("CARGO_PKG_VERSION"),
        chat_id.0,
    );

    bot.send_message(chat_id, text)
        .parse_mode(ParseMode::Html)
        .await?;
    Ok(())
}

async fn handle_help_command(bot: &Bot, chat_id: ChatId) -> ResponseResult<()> {
    let text = format!(
        "<b>Zymi v{}</b>\n\n\
         Just send a message to chat with the AI.\n\n\
         <b>Commands:</b>\n\
         /model — list available models\n\
         /model &lt;id&gt; — switch model\n\
         /clear — clear conversation history\n\
         /status — show bot status\n\
         /help — this message",
        env!("CARGO_PKG_VERSION"),
    );

    bot.send_message(chat_id, text)
        .parse_mode(ParseMode::Html)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// ask_user forwarder
// ---------------------------------------------------------------------------

async fn ask_user_forwarder(
    bot: Bot,
    mut ask_user_rx: mpsc::UnboundedReceiver<UserQuestion>,
    active_chat: ActiveChat,
    pending_questions: PendingQuestions,
) {
    while let Some(q) = ask_user_rx.recv().await {
        let chat_id = match *active_chat.lock().unwrap() {
            Some(id) => id,
            None => {
                log::warn!("ask_user: no active chat, dropping question");
                let _ = q.responder.send("Error: no active chat session".to_string());
                continue;
            }
        };

        let question_text = format!("❓ {}", q.question);
        if let Err(e) = bot.send_message(chat_id, &question_text).await {
            log::error!("ask_user: failed to send question to Telegram: {e}");
            let _ = q.responder.send(format!("Error sending question: {e}"));
            continue;
        }

        pending_questions.lock().unwrap().insert(chat_id, q.responder);
    }
}

// ---------------------------------------------------------------------------
// Message handler
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn gpt_handler(
    bot: Bot,
    msg: Message,
    agent: Arc<Agent>,
    provider_manager: Arc<ProviderManager>,
    pending: PendingApprovals,
    shared_approval: SharedApprovalHandler,
    start_time: std::time::Instant,
    pending_questions: PendingQuestions,
    active_chat: ActiveChat,
    transcription: SharedTranscription,
) -> ResponseResult<()> {
    let chat_id = msg.chat.id;

    // --- Cancel any pending ask_user question for this chat ---
    // This must happen BEFORE message-type branching so that photos and voice
    // messages also cancel a pending question (previously only text did).
    // For text messages the reply is forwarded to the waiting ask_user tool;
    // for other types we just cancel so the old agent call doesn't hang forever.
    {
        let is_text = msg.text().is_some();
        let responder = pending_questions.lock().unwrap().remove(&chat_id);
        if let Some(responder) = responder {
            if is_text {
                // Text reply: forward to the waiting ask_user and return early
                let text = msg.text().unwrap();
                log::info!("ask_user: received text reply for chat_id={}, len={}", chat_id, text.len());
                let _ = responder.send(text.to_string());
                return Ok(());
            }
            // Non-text (photo/voice): cancel the pending question so the old
            // agent call unblocks instead of hanging forever.
            log::info!("ask_user: cancelling pending question for chat_id={} (non-text message received)", chat_id);
            let _ = responder.send(
                "[MEDIA_RECEIVED] User sent a photo/media in response. \
                 Stop now — it will be processed as the next message."
                    .to_string(),
            );
        }
    }

    // --- Build user message from text, voice, or photo ---
    let user_message: crate::core::Message = if let Some(voice) = msg.voice() {
        // Voice message → transcribe to text
        let svc = match transcription.as_ref() {
            Some(svc) => svc,
            None => {
                bot.send_message(chat_id, "Voice transcription is not configured (OPENAI_API_KEY missing)")
                    .await?;
                return Ok(());
            }
        };

        let file = bot.get_file(voice.file.id.clone()).await?;
        let mut buf = Vec::new();
        bot.download_file(&file.path, &mut buf).await?;

        log::info!("Voice message: chat_id={}, file_size={}", chat_id, buf.len());

        match svc.transcribe(buf, "voice.ogg").await {
            Ok(text) => {
                // Show transcription to user
                let preview = format!("\u{1f3a4} {text}");
                bot.send_message(chat_id, &preview).await.ok();
                crate::core::Message::User(text)
            }
            Err(e) => {
                log::error!("Transcription failed: {e}");
                bot.send_message(chat_id, format!("Transcription failed: {e}"))
                    .await?;
                return Ok(());
            }
        }
    } else if let Some(photos) = msg.photo() {
        // Photo → base64 encode and send as multimodal
        let photo = match photos.last() {
            Some(p) => p,
            None => return Ok(()),
        };

        let file = bot.get_file(photo.file.id.clone()).await?;
        let mut buf = Vec::new();
        bot.download_file(&file.path, &mut buf).await?;

        log::info!("Photo message: chat_id={}, file_size={}", chat_id, buf.len());

        let b64 = base64::engine::general_purpose::STANDARD.encode(&buf);
        let mut parts = Vec::new();
        let caption = msg.caption().unwrap_or("What's in this image?");
        parts.push(ContentPart::Text(caption.to_string()));
        parts.push(ContentPart::ImageBase64 {
            media_type: "image/jpeg".to_string(),
            data: b64,
        });
        crate::core::Message::UserMultimodal { parts }
    } else if let Some(text) = msg.text() {
        let user_id = msg.from.as_ref().map(|u| u.id.0);
        log::info!(
            "Telegram message: user_id={:?}, chat_id={}, len={}",
            user_id,
            chat_id,
            text.len()
        );

        // Handle slash commands
        if let Some(cmd) = parse_slash_command(text) {
            return match cmd {
                SlashCommand::Model(id) => handle_model_command(&bot, chat_id, &provider_manager, id).await,
                SlashCommand::Clear => handle_clear_command(&bot, chat_id, &agent).await,
                SlashCommand::Status => handle_status_command(&bot, chat_id, &provider_manager, start_time).await,
                SlashCommand::Help => handle_help_command(&bot, chat_id).await,
            };
        }

        crate::core::Message::User(text.to_string())
    } else {
        // Unsupported message type
        return Ok(());
    };

    // Typing indicator is cosmetic — don't let it block the handler
    if let Err(e) = bot.send_chat_action(chat_id, teloxide::types::ChatAction::Typing).await {
        log::warn!("send_chat_action failed: {e}");
    }

    let conversation_id = format!("telegram:{}", chat_id.0);
    let approval_handler: Arc<dyn ApprovalHandler> =
        Arc::new(TelegramApprovalHandler::new(bot.clone(), chat_id, pending));

    let _guard = ApprovalSlotGuard::set(shared_approval, approval_handler.clone()).await;

    // Track active chat for ask_user routing
    {
        active_chat.lock().unwrap().replace(chat_id);
    }

    let start = std::time::Instant::now();
    let result = agent
        .process_multimodal(&conversation_id, user_message, Some(approval_handler.as_ref()))
        .await;

    // Clear active chat
    {
        let mut active = active_chat.lock().unwrap();
        if *active == Some(chat_id) {
            *active = None;
        }
    }

    match result {
        Ok(answer) if !answer.is_empty() => {
            log::info!(
                "Telegram reply: chat_id={}, {:?}, response_len={}",
                chat_id,
                start.elapsed(),
                answer.len()
            );
            send_long_message(&bot, chat_id, &answer).await?;
        }
        Ok(_) => {
            // Empty response (e.g. ask_user superseded by photo) — nothing to send
            log::info!("Telegram: chat_id={}, {:?}, suppressed empty response", chat_id, start.elapsed());
        }
        Err(e) => {
            log::error!("LLM error after {:?}: {e}", start.elapsed());
            let user_msg = match e {
                LlmError::RequestBuildError(_) => "Request build error",
                LlmError::ApiError(_) => "API error",
                LlmError::EmptyResponse => "Empty response from model",
                LlmError::StorageError(_) => "Storage error",
                LlmError::ApprovalError(_) => "Approval error",
            };
            bot.send_message(chat_id, user_msg).await?;
        }
    }

    Ok(())
}
