use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;

use tokio::sync::RwLock;

use crate::audit::{AuditEvent, AuditLog};
use crate::core::approval::ApprovalHandler;
use crate::core::langfuse::{self, LangfuseClient, TraceCtx};
use crate::core::tool_selector::ToolSelector;
use crate::core::{LlmError, LlmProvider, LlmResponse, Message, StreamEvent, ToolDefinition};
use crate::mcp::McpManager;
use crate::storage::ConversationStorage;
use crate::tools::Tool;
use crate::workflow::WorkflowEngine;

fn truncate_for_log(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let end = s.floor_char_boundary(max);
        format!("{}...", &s[..end])
    }
}

/// Build a dedup key for a tool call. For search tools, normalize the query
/// (lowercase + sort words) so near-duplicate queries like
/// "Familia Авиапарк магазин" and "магазин Familia Авиапарк" map to the same key.
fn tool_dedup_key(name: &str, arguments: &str) -> String {
    if name == "web_search" || name == "web_scrape" {
        if let Ok(args) = serde_json::from_str::<serde_json::Value>(arguments) {
            let field = if name == "web_search" { "query" } else { "url" };
            if let Some(query) = args.get(field).and_then(|v| v.as_str()) {
                let lower = query.to_lowercase();
                let mut words: Vec<&str> = lower.split_whitespace().collect();
                words.sort_unstable();
                words.dedup();
                return format!("{name}:{}", words.join(" "));
            }
        }
    }
    format!("{name}:{arguments}")
}

const DEFAULT_MAX_ITERATIONS: usize = 25;
const DEFAULT_MAX_REVIEWS: usize = 1;
const DEFAULT_SUMMARY_THRESHOLD: usize = 80;
const MAX_TOOL_OUTPUT: usize = 50_000;
const KEEP_RECENT_MESSAGES: usize = 20;

const SUMMARY_PROMPT: &str = "\
Summarize the following conversation between a user and an AI assistant. \
Extract and preserve:\n\
- Key decisions and agreements\n\
- Important facts, names, URLs, and technical details\n\
- Current task status and pending items\n\n\
Be concise but complete. This summary replaces the conversation history \
and will be the only context available for future interactions.\n\
Do NOT include user preferences or personality observations here — \
those are extracted separately.";

const PREFERENCES_PROMPT: &str = "\
You are analyzing a conversation to extract lasting insights about the user. \
Focus ONLY on things that will remain relevant across future conversations.\n\n\
Extract:\n\
- Communication style and language preferences (language, formality, brevity)\n\
- Domain expertise and professional context\n\
- Recurring interests, hobbies, personal facts\n\
- Workflow preferences (tools, formats, approaches they prefer)\n\
- Pet peeves, things they dislike or explicitly asked to avoid\n\
- Important names, places, projects they reference regularly\n\n\
Rules:\n\
- Be concise: one line per insight, use bullet points\n\
- Skip anything task-specific or temporary\n\
- Skip obvious things (e.g. \"user asks questions\" — of course they do)\n\
- If the conversation reveals nothing new about the user, respond with exactly: NO_UPDATE\n\
- If previous preferences are provided, merge new insights into them: \
update existing items if new info refines them, add new items, \
remove items that are clearly contradicted. Return the full updated list.\n\
- Write in the same language the user primarily speaks in the conversation.";

const MONITOR_SYSTEM_PROMPT: &str = "\
You are a response quality monitor. Your job is to evaluate the assistant's proposed response \
in the context of the conversation.

Evaluate the response for:
1. Correctness — does it accurately address the user's question/request?
2. Completeness — does it cover all aspects the user asked about?
3. Clarity — is it well-structured and easy to understand?
4. Relevance — does it stay on topic without unnecessary tangents?

If the response is acceptable, reply with exactly: APPROVED

If the response needs improvement, provide specific, actionable feedback on what should be changed. \
Do NOT rewrite the response yourself — just describe what needs fixing.";

const EXTRACT_FACTS_PROMPT: &str = "\
You are analyzing a single user message in the context of an ongoing conversation. \
Extract any durable, interesting facts that would be worth remembering across future conversations.\n\n\
Focus on:\n\
- Personal facts: names, relationships, locations, birthdays, pets\n\
- Professional facts: projects, employers, clients, deadlines, tech stack\n\
- Preferences and opinions beyond communication style (favorite restaurants, hobbies, brands)\n\
- Important decisions, plans, goals, or commitments the user mentions\n\
- Domain knowledge: specific terms, systems, processes the user refers to\n\
- Any concrete data points (server IPs, project names, repo URLs, account names)\n\n\
Rules:\n\
- Return one fact per line, as a bullet point (- fact)\n\
- Each fact should be self-contained and understandable without conversation context\n\
- Skip anything that is task-specific instructions to you (e.g. \"fix this bug\" is not a fact)\n\
- Skip things already present in the existing facts list provided below\n\
- If no new durable facts are found, respond with exactly: NO_FACTS\n\
- Write facts in the same language as the user's message.";

const CONSOLIDATE_FACTS_PROMPT: &str = "\
You are consolidating a list of facts about a user. \
Merge duplicates, remove contradicted facts (keep the newer version), \
and organize into logical groups (personal, professional, technical, etc.).\n\n\
Rules:\n\
- One fact per line, bullet point format\n\
- Remove date headers, just keep the clean facts\n\
- Preserve all unique, non-contradicted information\n\
- Write in the same language as the original facts.";

fn is_trivial_reply(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    const TRIVIAL: &[&str] = &[
        "да", "нет", "ок", "ok", "yes", "no", "ага", "угу",
        "спасибо", "thanks", "thank you", "good", "хорошо", "понял",
        "ладно", "давай", "go", "got it", "👍", "👎", "+", "-",
        "продолжай", "continue", "далее", "next", "дальше",
    ];
    TRIVIAL.iter().any(|t| lower == *t)
}

pub struct MonitorConfig {
    pub provider: Arc<dyn LlmProvider>,
    pub max_reviews: usize,
}

impl MonitorConfig {
    pub fn new(provider: Arc<dyn LlmProvider>) -> Self {
        Self {
            provider,
            max_reviews: DEFAULT_MAX_REVIEWS,
        }
    }
}

/// Sanitize conversation history by:
/// 1. Removing orphaned ToolResults whose tool_call is missing (e.g. after summarization)
/// 2. Removing trailing Assistant messages with tool_calls that lack corresponding results
///    (e.g. after a crash between saving assistant message and tool results)
fn sanitize_history(messages: &mut Vec<Message>) {
    // Pass 1: remove orphaned ToolResults (tool_call_id not found in any Assistant)
    let all_call_ids: HashSet<String> = messages
        .iter()
        .filter_map(|m| {
            if let Message::Assistant { tool_calls, .. } = m {
                Some(tool_calls.iter().map(|tc| tc.id.clone()))
            } else {
                None
            }
        })
        .flatten()
        .collect();

    let before = messages.len();
    messages.retain(|m| {
        if let Message::ToolResult { tool_call_id, .. } = m {
            all_call_ids.contains(tool_call_id)
        } else {
            true
        }
    });
    if messages.len() < before {
        log::warn!(
            "Sanitizing history: removed {} orphaned tool result(s)",
            before - messages.len()
        );
    }

    // Pass 2: remove trailing Assistant messages with missing tool results
    loop {
        let last_assistant_idx = messages.iter().rposition(|m| matches!(m, Message::Assistant { tool_calls, .. } if !tool_calls.is_empty()));

        let Some(idx) = last_assistant_idx else {
            break;
        };

        let expected_ids: HashSet<String> = if let Message::Assistant { tool_calls, .. } = &messages[idx] {
            tool_calls.iter().map(|tc| tc.id.clone()).collect()
        } else {
            break;
        };

        let actual_ids: HashSet<String> = messages[idx + 1..]
            .iter()
            .filter_map(|m| {
                if let Message::ToolResult { tool_call_id, .. } = m {
                    Some(tool_call_id.clone())
                } else {
                    None
                }
            })
            .collect();

        if expected_ids.is_subset(&actual_ids) {
            break;
        }

        let missing: Vec<_> = expected_ids.difference(&actual_ids).collect();
        log::warn!(
            "Sanitizing history: removing assistant message at index {} with {} orphaned tool_call(s): {:?}",
            idx,
            missing.len(),
            missing
        );
        messages.truncate(idx);
    }
}

pub struct Agent {
    provider: Arc<dyn LlmProvider>,
    tools: RwLock<Vec<Arc<dyn Tool>>>,
    system_prompt: Option<String>,
    storage: Arc<dyn ConversationStorage>,
    max_iterations: usize,
    summary_threshold: usize,
    monitor: Option<MonitorConfig>,
    memory_dir: Option<PathBuf>,
    workflow_engine: Option<Arc<WorkflowEngine>>,
    langfuse: Option<(Arc<LangfuseClient>, String)>,
    audit: Option<AuditLog>,
    tool_selector: RwLock<Option<ToolSelector>>,
    auto_extract: bool,
    last_extract_time: std::sync::Mutex<std::time::Instant>,
    mcp_manager: Option<Arc<tokio::sync::Mutex<McpManager>>>,
}

impl Agent {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        tools: Vec<Box<dyn Tool>>,
        system_prompt: Option<String>,
        storage: Arc<dyn ConversationStorage>,
    ) -> Self {
        let arc_tools: Vec<Arc<dyn Tool>> = tools.into_iter().map(Arc::from).collect();
        Self {
            provider,
            tools: RwLock::new(arc_tools),
            system_prompt,
            storage,
            max_iterations: DEFAULT_MAX_ITERATIONS,
            summary_threshold: DEFAULT_SUMMARY_THRESHOLD,
            monitor: None,
            memory_dir: None,
            workflow_engine: None,
            langfuse: None,
            audit: None,
            tool_selector: RwLock::new(None),
            auto_extract: false,
            last_extract_time: std::sync::Mutex::new(
                std::time::Instant::now() - std::time::Duration::from_secs(60)
            ),
            mcp_manager: None,
        }
    }

    pub fn with_audit(mut self, audit: AuditLog) -> Self {
        self.audit = Some(audit);
        self
    }

    pub fn with_max_iterations(mut self, max: usize) -> Self {
        self.max_iterations = max;
        self
    }

    pub fn with_summary_threshold(mut self, threshold: usize) -> Self {
        self.summary_threshold = threshold;
        self
    }

    pub fn with_monitor(mut self, config: MonitorConfig) -> Self {
        self.monitor = Some(config);
        self
    }

    pub fn with_memory_dir(mut self, dir: PathBuf) -> Self {
        self.memory_dir = Some(dir);
        self
    }

    pub fn with_workflow_engine(mut self, engine: Arc<WorkflowEngine>) -> Self {
        self.workflow_engine = Some(engine);
        self
    }

    pub fn with_langfuse(mut self, client: Arc<LangfuseClient>, model_name: String) -> Self {
        self.langfuse = Some((client, model_name));
        self
    }

    pub fn with_tool_selector(self, selector: ToolSelector) -> Self {
        *self.tool_selector.blocking_write() = Some(selector);
        self
    }

    pub fn with_mcp_manager(mut self, manager: Arc<tokio::sync::Mutex<McpManager>>) -> Self {
        self.mcp_manager = Some(manager);
        self
    }

    pub fn with_auto_extract(mut self, enabled: bool) -> Self {
        self.auto_extract = enabled;
        self
    }

    /// Register multiple tools at runtime in a single batch embedding call.
    pub async fn register_tools(&self, new_tools: Vec<Box<dyn Tool>>) {
        if new_tools.is_empty() {
            return;
        }
        let defs: Vec<ToolDefinition> = new_tools.iter().map(|t| t.definition()).collect();
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        {
            let mut tools = self.tools.write().await;
            let name_set: HashSet<&str> = names.iter().copied().collect();
            tools.retain(|t| !name_set.contains(t.definition().name.as_str()));
            for tool in new_tools {
                tools.push(Arc::from(tool));
            }
        }
        {
            let mut selector = self.tool_selector.write().await;
            if let Some(ref mut sel) = *selector {
                if let Err(e) = sel.add_tools(&defs).await {
                    log::warn!("Failed to embed {} new tools: {e}", defs.len());
                }
            }
        }
        log::info!("Registered {} tools at runtime: [{}]", names.len(), names.join(", "));
    }

    /// Post-tool-call hook: trigger MCP hot-reload when manage_mcp adds a server.
    async fn post_tool_hook(&self, tool_name: &str, arguments: &str, result: &str) {
        if tool_name == "manage_mcp" && !result.starts_with("Tool error:") {
            if let Ok(args) = serde_json::from_str::<serde_json::Value>(arguments) {
                if args.get("action").and_then(|v| v.as_str()) == Some("add") {
                    if let Some(name) = args.get("name").and_then(|v| v.as_str()) {
                        self.connect_new_mcp_servers(&[name.to_string()]).await;
                    }
                }
            }
        }
    }

    /// Connect new MCP servers by name and register their tools.
    async fn connect_new_mcp_servers(&self, server_names: &[String]) {
        let memory_dir = match &self.memory_dir {
            Some(d) => d.clone(),
            None => return,
        };
        let mcp_manager = match &self.mcp_manager {
            Some(m) => m.clone(),
            None => {
                log::warn!("Cannot connect MCP servers: no McpManager available");
                return;
            }
        };

        let mut manager = mcp_manager.lock().await;
        let new_tools = manager.connect_servers_by_name(&memory_dir, server_names).await;
        drop(manager);

        if !new_tools.is_empty() {
            log::info!(
                "Connected {} new MCP tools from servers: [{}]",
                new_tools.len(),
                server_names.join(", ")
            );
            self.register_tools(new_tools).await;
        }
    }

    /// Clear conversation history for a given conversation_id.
    pub async fn clear_history(&self, conversation_id: &str) -> Result<(), LlmError> {
        self.storage.clear(conversation_id).await
            .map_err(|e| LlmError::StorageError(e.to_string()))
    }

    fn trace_ctx(&self, conversation_id: &str, user_message: &str) -> Option<TraceCtx> {
        let (client, model_name) = self.langfuse.as_ref()?;
        Some(TraceCtx::new(client.clone(), model_name, conversation_id, user_message))
    }

    fn trace_generation(&self, trace: Option<&TraceCtx>, messages: &[Message], response: &LlmResponse, start: &str, end: &str) {
        let Some(t) = trace else { return };
        let usage = response.usage.as_ref();
        let tool_names: Vec<&str> = response.tool_calls.iter().map(|tc| tc.name.as_str()).collect();
        t.record_generation(
            messages,
            response.content.as_deref(),
            &tool_names,
            usage.map_or(0, |u| u.input_tokens),
            usage.map_or(0, |u| u.output_tokens),
            start,
            end,
        );
    }

    /// Load previous conversation summary from memory if it exists.
    fn load_summary(&self, conversation_id: &str) -> Option<String> {
        let memory_dir = self.memory_dir.as_ref()?;
        let path = memory_dir
            .join("conversations")
            .join(format!("{conversation_id}.md"));
        let content = std::fs::read_to_string(path).ok()?;
        if content.trim().is_empty() {
            None
        } else {
            log::info!(
                "Loaded conversation summary for '{}' ({} chars)",
                conversation_id,
                content.len()
            );
            Some(content)
        }
    }

    /// Get tool definitions, optionally filtered by the tool selector.
    async fn get_tool_definitions(&self, query: &str) -> Vec<ToolDefinition> {
        let all: Vec<ToolDefinition> = {
            let tools = self.tools.read().await;
            tools.iter().map(|t| t.definition()).collect()
        };
        let selector = self.tool_selector.read().await;
        if let Some(ref sel) = *selector {
            match sel.select_tools(query, &all).await {
                Ok(selected) => selected,
                Err(e) => {
                    log::warn!("Tool selection failed: {e}, using all tools");
                    all
                }
            }
        } else {
            all
        }
    }

    /// Load extracted facts from long-term memory.
    fn load_facts(&self) -> Option<String> {
        let memory_dir = self.memory_dir.as_ref()?;
        let path = memory_dir.join("facts.md");
        let content = std::fs::read_to_string(path).ok()?;
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return None;
        }
        // Truncate if too large (keep last ~4000 chars)
        let facts = if trimmed.len() > 4000 {
            let start = trimmed.len() - 4000;
            let boundary = trimmed.ceil_char_boundary(start);
            &trimmed[boundary..]
        } else {
            trimmed
        };
        log::info!("Loaded user facts ({} chars)", facts.len());
        Some(facts.to_string())
    }

    /// Spawn background fact extraction if auto_extract is enabled and rate limit allows.
    fn spawn_extract(&self, user_message: &str) {
        if !self.auto_extract {
            return;
        }
        let memory_dir = match &self.memory_dir {
            Some(dir) => dir.clone(),
            None => return,
        };

        // Rate limit: skip if last extraction was less than 30 seconds ago
        {
            let mut last = self.last_extract_time.lock().unwrap();
            let now = std::time::Instant::now();
            if now.duration_since(*last) < std::time::Duration::from_secs(30) {
                log::debug!("Auto-extract: rate limited, skipping");
                return;
            }
            *last = now;
        }

        // Heuristic pre-filter: skip trivial messages
        let trimmed = user_message.trim();
        if trimmed.len() < 20 || trimmed.starts_with('/') || is_trivial_reply(trimmed) {
            log::debug!("Auto-extract: message too short/trivial, skipping");
            return;
        }

        let provider = self.provider.clone();
        let msg = user_message.to_string();

        tokio::spawn(async move {
            extract_facts(provider, &memory_dir, &msg).await;
        });
    }

    /// Load user preferences from memory if available.
    fn load_preferences(&self) -> Option<String> {
        let memory_dir = self.memory_dir.as_ref()?;
        let path = memory_dir.join("preferences.md");
        let content = std::fs::read_to_string(path).ok()?;
        let trimmed = content.trim();
        // Skip if file is empty or only has the header
        if trimmed.is_empty() || trimmed == "# User Preferences" {
            None
        } else {
            log::info!("Loaded user preferences ({} chars)", content.len());
            Some(content)
        }
    }

    /// Spawn background summarization if conversation exceeds threshold.
    fn spawn_summarize(&self, conversation_id: &str) {
        let memory_dir = match &self.memory_dir {
            Some(dir) => dir.clone(),
            None => return,
        };
        let provider = self.provider.clone();
        let storage = self.storage.clone();
        let conv_id = conversation_id.to_string();
        let threshold = self.summary_threshold;

        tokio::spawn(async move {
            summarize_conversation(provider, storage, &memory_dir, &conv_id, threshold).await;
        });
    }

    /// Run the monitor to evaluate a proposed response.
    /// Returns `Some(feedback)` if revision is needed, `None` if approved or on error.
    async fn run_monitor(&self, messages: &[Message], proposed: &str) -> Option<String> {
        let monitor = self.monitor.as_ref()?;

        let mut monitor_messages = vec![Message::System(MONITOR_SYSTEM_PROMPT.to_string())];

        // Include conversation context (skip system prompt — monitor has its own)
        for msg in messages {
            if matches!(msg, Message::System(_)) {
                continue;
            }
            monitor_messages.push(msg.clone());
        }

        // Add the proposed response for evaluation
        monitor_messages.push(Message::User(format!(
            "[Proposed assistant response to evaluate]\n\n{proposed}"
        )));

        match monitor.provider.chat(&monitor_messages, &[]).await {
            Ok(response) => {
                if let Some(content) = response.content {
                    let trimmed = content.trim();
                    if trimmed.starts_with("APPROVED") {
                        log::info!("Monitor approved response");
                        None
                    } else {
                        log::info!("Monitor requested revision: {}", truncate_for_log(trimmed, 100));
                        Some(content)
                    }
                } else {
                    log::warn!("Monitor returned empty response, approving by default");
                    None
                }
            }
            Err(e) => {
                log::warn!("Monitor error (approving by default): {e}");
                None
            }
        }
    }

    /// Prepare conversation messages: store user message, load & sanitize history,
    /// prepend system prompt / preferences / summary.
    /// Returns (messages, history_message_count).
    async fn prepare_messages(
        &self,
        conversation_id: &str,
        user_message: Message,
    ) -> Result<(Vec<Message>, usize), LlmError> {
        self.storage
            .add_message(
                conversation_id,
                &user_message,
            )
            .await
            .map_err(|e| LlmError::StorageError(e.to_string()))?;

        let mut history = self
            .storage
            .get_history(conversation_id)
            .await
            .map_err(|e| LlmError::StorageError(e.to_string()))?;

        let history_len_before = history.len();
        sanitize_history(&mut history);
        if history.len() != history_len_before {
            log::warn!(
                "History sanitized: {} -> {} messages",
                history_len_before,
                history.len()
            );
            self.storage
                .clear(conversation_id)
                .await
                .map_err(|e| LlmError::StorageError(e.to_string()))?;
            for msg in &history {
                self.storage
                    .add_message(conversation_id, msg)
                    .await
                    .map_err(|e| LlmError::StorageError(e.to_string()))?;
            }
        }

        let mut messages = Vec::new();

        if let Some(ref prompt) = self.system_prompt {
            messages.push(Message::System(prompt.clone()));
        }

        messages.push(Message::System(system_info()));

        if let Some(prefs) = self.load_preferences() {
            messages.push(Message::System(format!(
                "[User preferences — already loaded, do NOT re-read via read_memory]\n\n{prefs}"
            )));
        }

        if let Some(facts) = self.load_facts() {
            messages.push(Message::System(format!(
                "[Known facts about user — already loaded, do NOT re-read via read_memory]\n\n{facts}"
            )));
        }

        // Fire-and-forget fact extraction from user message
        if let Some(text) = user_message.user_text() {
            self.spawn_extract(text);
        }

        if let Some(summary) = self.load_summary(conversation_id) {
            messages.push(Message::System(format!(
                "[Previous conversation context — already loaded, do NOT re-read via read_memory]\n\n{summary}"
            )));
        }

        let history_len = history.len();
        messages.extend(history);
        Ok((messages, history_len))
    }

    /// Execute a single tool call: dedup check, approval, execution.
    /// Returns `(result_text, is_duplicate)`.
    async fn execute_tool_call(
        &self,
        tool_call: &crate::core::ToolCallInfo,
        explanation: Option<&str>,
        approval_handler: Option<&dyn ApprovalHandler>,
        tool_call_cache: &mut HashMap<String, ()>,
    ) -> Result<(String, bool), LlmError> {
        let dedup_key = tool_dedup_key(&tool_call.name, &tool_call.arguments);
        if tool_call_cache.contains_key(&dedup_key) {
            log::warn!("Duplicate tool call: {}", tool_call.name);
            return Ok((
                "[Duplicate call] You already made this or a very similar search. \
                Do NOT repeat it. Use the results you already have to formulate your answer."
                    .to_string(),
                true,
            ));
        }

        let tool_start = std::time::Instant::now();

        // Clone the Arc so we don't hold the read lock during async execution
        let tool_arc: Option<Arc<dyn Tool>> = {
            let tools = self.tools.read().await;
            tools.iter().find(|t| t.definition().name == tool_call.name).cloned()
        };

        let result = match tool_arc {
            Some(ref tool) => {
                if tool.requires_approval_for(&tool_call.arguments) {
                    let approved = if let Some(handler) = approval_handler {
                        let description = tool.format_approval_request(&tool_call.arguments);
                        handler
                            .request_approval(&description, explanation)
                            .await
                            .map_err(LlmError::ApprovalError)?
                    } else {
                        log::warn!(
                            "Tool '{}' requires approval but no handler available",
                            tool_call.name
                        );
                        false
                    };

                    if !approved {
                        "Tool execution was rejected by user. Try an alternative approach if possible.".to_string()
                    } else {
                        tool.execute(&tool_call.arguments).await.unwrap_or_else(|e| {
                            log::error!("Tool '{}' error: {}", tool_call.name, e);
                            format!("Tool error: {e}\n\n[Hint: try a different approach or alternative command before giving up.]")
                        })
                    }
                } else {
                    tool.execute(&tool_call.arguments).await.unwrap_or_else(|e| {
                        log::error!("Tool '{}' error: {}", tool_call.name, e);
                        format!("Tool error: {e}\n\n[Hint: try a different approach or alternative command before giving up.]")
                    })
                }
            }
            None => {
                log::warn!("Unknown tool requested: {}", tool_call.name);
                let available: Vec<String> = {
                    let tools = self.tools.read().await;
                    tools.iter().map(|t| t.definition().name).collect()
                };
                format!(
                    "Unknown tool: '{}'. Available tools: [{}]. Use one of these instead.",
                    tool_call.name,
                    available.join(", ")
                )
            }
        };

        log::info!(
            "Tool '{}' completed: {:?}, result_len={}",
            tool_call.name,
            tool_start.elapsed(),
            result.len()
        );
        log::debug!(
            "Tool '{}' result: {}",
            tool_call.name,
            truncate_for_log(&result, 300)
        );

        // Audit log
        if let Some(ref audit) = self.audit {
            let is_error = result.starts_with("Tool error:") || result.starts_with("Tool execution was rejected");
            audit.log(AuditEvent::ToolCall {
                tool: tool_call.name.clone(),
                arguments: truncate_for_log(&tool_call.arguments, 500),
                result_preview: truncate_for_log(&result, 200),
                is_error,
            });
        }

        tool_call_cache.insert(dedup_key, ());
        Ok((result, false))
    }

    /// Store a tool result in both the message list and persistent storage.
    async fn store_tool_result(
        &self,
        conversation_id: &str,
        tool_call_id: &str,
        result: String,
        messages: &mut Vec<Message>,
    ) -> Result<(), LlmError> {
        let content = if result.len() > MAX_TOOL_OUTPUT {
            let end = result.floor_char_boundary(MAX_TOOL_OUTPUT);
            format!(
                "{}\n\n[Output truncated at {} characters]",
                &result[..end],
                MAX_TOOL_OUTPUT
            )
        } else {
            result
        };
        let msg = Message::ToolResult {
            tool_call_id: tool_call_id.to_string(),
            content,
        };
        self.storage
            .add_message(conversation_id, &msg)
            .await
            .map_err(|e| LlmError::StorageError(e.to_string()))?;
        messages.push(msg);
        Ok(())
    }

    /// Store final assistant response and trigger summarization.
    async fn finalize_response(
        &self,
        conversation_id: &str,
        content: &str,
    ) -> Result<(), LlmError> {
        self.storage
            .add_message(
                conversation_id,
                &Message::Assistant {
                    content: Some(content.to_string()),
                    tool_calls: vec![],
                },
            )
            .await
            .map_err(|e| LlmError::StorageError(e.to_string()))?;
        self.spawn_summarize(conversation_id);
        Ok(())
    }

    pub async fn process_multimodal(
        &self,
        conversation_id: &str,
        user_message: Message,
        approval_handler: Option<&dyn ApprovalHandler>,
    ) -> Result<String, LlmError> {
        let user_text = user_message.user_text().unwrap_or("").to_string();
        log::info!(
            "Agent process: conversation_id={}, message_len={}",
            conversation_id,
            user_text.len()
        );

        let trace = self.trace_ctx(conversation_id, &user_text);

        let (mut messages, _history_len) = self.prepare_messages(conversation_id, user_message).await?;
        let tool_definitions = self.get_tool_definitions(&user_text).await;
        let mut monitor_reviews: usize = 0;
        let mut tool_call_cache: HashMap<String, ()> = HashMap::new();

        for iteration in 0..self.max_iterations {
            log::info!("Iteration {}/{}", iteration + 1, self.max_iterations);

            let llm_start = langfuse::timestamp();
            let response = self.provider.chat(&messages, &tool_definitions).await?;
            let llm_end = langfuse::timestamp();
            self.trace_generation(trace.as_ref(), &messages, &response, &llm_start, &llm_end);

            if response.tool_calls.is_empty() {
                let content = response.content.ok_or(LlmError::EmptyResponse)?;

                let max_reviews = self.monitor.as_ref().map_or(0, |m| m.max_reviews);
                if monitor_reviews < max_reviews {
                    log::info!("Monitor: draft response ({} chars):\n{content}", content.len());
                    if let Some(feedback) = self.run_monitor(&messages, &content).await {
                        log::info!("Monitor: feedback:\n{feedback}");
                        messages.push(Message::User(format!(
                            "[INTERNAL — revision required]\n\
                            Your draft response was:\n---\n{content}\n---\n\n\
                            Quality feedback:\n{feedback}\n\n\
                            Rewrite your response to the user's original message. \
                            Output ONLY the improved response, nothing else."
                        )));
                        monitor_reviews += 1;
                        continue;
                    }
                }

                if monitor_reviews > 0 {
                    log::info!("Monitor: final response after {} revision(s) ({} chars):\n{content}", monitor_reviews, content.len());
                }

                if let Some(ref t) = trace { t.finish(&content); }
                self.finalize_response(conversation_id, &content).await?;
                return Ok(content);
            }

            let explanation = response.content.clone();
            let assistant_msg = Message::Assistant {
                content: response.content,
                tool_calls: response.tool_calls.clone(),
            };
            self.storage
                .add_message(conversation_id, &assistant_msg)
                .await
                .map_err(|e| LlmError::StorageError(e.to_string()))?;
            messages.push(assistant_msg);

            for tool_call in &response.tool_calls {
                log::info!(
                    "Tool call: {} | args: {}",
                    tool_call.name,
                    truncate_for_log(&tool_call.arguments, 200)
                );

                let tool_start = langfuse::timestamp();
                let (result, is_dup) = self
                    .execute_tool_call(
                        tool_call,
                        explanation.as_deref(),
                        approval_handler,
                        &mut tool_call_cache,
                    )
                    .await?;
                let tool_end = langfuse::timestamp();

                if let Some(ref t) = trace {
                    let is_error = is_dup
                        || result.starts_with("Tool error:")
                        || result.starts_with("Unknown tool:");
                    t.record_tool(&tool_call.name, &tool_call.arguments, &result, is_error, &tool_start, &tool_end);
                }

                self.post_tool_hook(&tool_call.name, &tool_call.arguments, &result).await;

                self.store_tool_result(conversation_id, &tool_call.id, result, &mut messages)
                    .await?;
            }
        }

        // Forced conclusion
        log::warn!("Max iterations ({}) exceeded, forcing conclusion", self.max_iterations);
        let llm_start = langfuse::timestamp();
        let response = self.provider.chat(&messages, &[]).await?;
        let llm_end = langfuse::timestamp();
        self.trace_generation(trace.as_ref(), &messages, &response, &llm_start, &llm_end);

        let content = response
            .content
            .unwrap_or_else(|| "I was unable to complete the task within the iteration limit.".to_string());

        if let Some(ref t) = trace { t.finish(&content); }
        self.finalize_response(conversation_id, &content).await?;
        Ok(content)
    }

    pub async fn process(
        &self,
        conversation_id: &str,
        user_message: &str,
        approval_handler: Option<&dyn ApprovalHandler>,
    ) -> Result<String, LlmError> {
        self.process_multimodal(
            conversation_id,
            Message::User(user_message.to_string()),
            approval_handler,
        )
        .await
    }

    pub async fn process_stream(
        &self,
        conversation_id: &str,
        user_message: &str,
        approval_handler: Option<&dyn ApprovalHandler>,
        event_tx: mpsc::UnboundedSender<StreamEvent>,
    ) -> Result<String, LlmError> {
        log::info!(
            "Agent process_stream: conversation_id={}, message_len={}",
            conversation_id,
            user_message.len()
        );

        let trace = self.trace_ctx(conversation_id, user_message);

        // --- Workflow engine routing ---
        if let Some(ref engine) = self.workflow_engine {
            match engine.process(user_message, event_tx.clone()).await {
                Ok(result) => {
                    // Connect new MCP servers discovered by workflow
                    if !result.new_mcp_servers.is_empty() {
                        self.connect_new_mcp_servers(&result.new_mcp_servers).await;
                    }
                    if let Some(ref t) = trace { t.finish(&result.response); }
                    self.finalize_response(conversation_id, &result.response).await?;
                    let _ = event_tx.send(StreamEvent::Token(result.response.clone()));
                    let _ = event_tx.send(StreamEvent::ContentDone(result.response.clone()));
                    let _ = event_tx.send(StreamEvent::Done(result.response.clone()));
                    return Ok(result.response);
                }
                Err(crate::workflow::WorkflowError::SimpleTask { score }) => {
                    log::info!("Workflow: simple task (score {score}), using standard agent");
                }
                Err(e) => {
                    log::error!("Workflow engine error: {e}, falling back to standard agent");
                }
            }
        }

        let (mut messages, history_len) = self.prepare_messages(conversation_id, Message::User(user_message.to_string())).await?;
        let tool_definitions = self.get_tool_definitions(user_message).await;
        let mut monitor_reviews: usize = 0;
        let mut tool_call_cache: HashMap<String, ()> = HashMap::new();
        let mut msg_count = history_len;

        for iteration in 0..self.max_iterations {
            log::info!("Stream iteration {}/{}", iteration + 1, self.max_iterations);
            let _ = event_tx.send(StreamEvent::IterationStart(iteration));

            let max_reviews = self.monitor.as_ref().map_or(0, |m| m.max_reviews);
            let pending_review = monitor_reviews < max_reviews;

            let llm_start = langfuse::timestamp();
            let response = if pending_review {
                self.provider.chat(&messages, &tool_definitions).await?
            } else {
                self.provider
                    .chat_stream(&messages, &tool_definitions, event_tx.clone())
                    .await?
            };
            let llm_end = langfuse::timestamp();
            self.trace_generation(trace.as_ref(), &messages, &response, &llm_start, &llm_end);

            if let Some(ref usage) = response.usage {
                let _ = event_tx.send(StreamEvent::Usage {
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    message_count: msg_count,
                    summary_threshold: self.summary_threshold,
                });
            }

            if response.tool_calls.is_empty() {
                let content = response.content.ok_or(LlmError::EmptyResponse)?;

                if pending_review {
                    log::info!("Monitor: draft response ({} chars):\n{content}", content.len());
                    if let Some(feedback) = self.run_monitor(&messages, &content).await {
                        log::info!("Monitor: feedback:\n{feedback}");
                        messages.push(Message::User(format!(
                            "[INTERNAL — revision required]\n\
                            Your draft response was:\n---\n{content}\n---\n\n\
                            Quality feedback:\n{feedback}\n\n\
                            Rewrite your response to the user's original message. \
                            Output ONLY the improved response, nothing else."
                        )));
                        monitor_reviews += 1;
                        continue;
                    }
                    let _ = event_tx.send(StreamEvent::Token(content.clone()));
                    let _ = event_tx.send(StreamEvent::ContentDone(content.clone()));
                }

                if monitor_reviews > 0 {
                    log::info!("Monitor: final response after {} revision(s) ({} chars):\n{content}", monitor_reviews, content.len());
                }

                if let Some(ref t) = trace { t.finish(&content); }
                self.finalize_response(conversation_id, &content).await?;
                let _ = event_tx.send(StreamEvent::Done(content.clone()));
                return Ok(content);
            }

            let explanation = response.content.clone();
            let assistant_msg = Message::Assistant {
                content: response.content,
                tool_calls: response.tool_calls.clone(),
            };
            self.storage
                .add_message(conversation_id, &assistant_msg)
                .await
                .map_err(|e| LlmError::StorageError(e.to_string()))?;
            messages.push(assistant_msg);
            msg_count += 1;

            for tool_call in &response.tool_calls {
                log::info!(
                    "Tool call: {} | args: {}",
                    tool_call.name,
                    truncate_for_log(&tool_call.arguments, 200)
                );

                let _ = event_tx.send(StreamEvent::ToolCallStart {
                    id: tool_call.id.clone(),
                    name: tool_call.name.clone(),
                    arguments: tool_call.arguments.clone(),
                });

                let tool_start = langfuse::timestamp();
                let (result, is_dup) = self
                    .execute_tool_call(
                        tool_call,
                        explanation.as_deref(),
                        approval_handler,
                        &mut tool_call_cache,
                    )
                    .await?;
                let tool_end = langfuse::timestamp();

                let is_error = is_dup
                    || result.starts_with("Tool error:")
                    || result.starts_with("Unknown tool:");

                if let Some(ref t) = trace {
                    t.record_tool(&tool_call.name, &tool_call.arguments, &result, is_error, &tool_start, &tool_end);
                }

                let _ = event_tx.send(StreamEvent::ToolCallResult {
                    id: tool_call.id.clone(),
                    name: tool_call.name.clone(),
                    result: result.clone(),
                    is_error,
                });

                self.post_tool_hook(&tool_call.name, &tool_call.arguments, &result).await;

                self.store_tool_result(conversation_id, &tool_call.id, result, &mut messages)
                    .await?;
                msg_count += 1;
            }
        }

        // Forced conclusion
        log::warn!("Stream: max iterations ({}) exceeded, forcing conclusion", self.max_iterations);
        let llm_start = langfuse::timestamp();
        let response = self
            .provider
            .chat_stream(&messages, &[], event_tx.clone())
            .await?;
        let llm_end = langfuse::timestamp();
        self.trace_generation(trace.as_ref(), &messages, &response, &llm_start, &llm_end);

        let content = response
            .content
            .unwrap_or_else(|| "I was unable to complete the task within the iteration limit.".to_string());

        if let Some(ref t) = trace { t.finish(&content); }
        self.finalize_response(conversation_id, &content).await?;
        let _ = event_tx.send(StreamEvent::Done(content.clone()));
        Ok(content)
    }
}

/// Background task: summarize a conversation if it exceeds the threshold.
/// Uses sliding window: only older messages are summarized, recent ones are kept intact.
async fn summarize_conversation(
    provider: Arc<dyn LlmProvider>,
    storage: Arc<dyn ConversationStorage>,
    memory_dir: &std::path::Path,
    conversation_id: &str,
    summary_threshold: usize,
) {
    let history = match storage.get_history(conversation_id).await {
        Ok(h) => h,
        Err(_) => return,
    };

    if history.len() < summary_threshold {
        return;
    }

    // Split: summarize the older part, keep the recent part.
    // Adjust split point so we never orphan a ToolResult from its tool_call.
    let mut split_point = history.len().saturating_sub(KEEP_RECENT_MESSAGES);
    while split_point > 0 {
        if let Message::ToolResult { .. } = &history[split_point] {
            split_point -= 1;
        } else {
            break;
        }
    }
    let to_summarize = &history[..split_point];
    let to_keep: Vec<Message> = history[split_point..].to_vec();

    log::info!(
        "Conversation '{}' has {} messages, summarizing (keeping last {})...",
        conversation_id,
        history.len(),
        to_keep.len(),
    );

    let summary_dir = memory_dir.join("conversations");
    let summary_path = summary_dir.join(format!("{conversation_id}.md"));
    let prefs_path = memory_dir.join("preferences.md");

    // Include previous summary so we don't lose older context
    let previous = std::fs::read_to_string(&summary_path).unwrap_or_default();

    let formatted_history = format_history_for_summary(to_summarize);

    // Build summary input
    let mut summary_input = String::new();
    if !previous.trim().is_empty() {
        summary_input.push_str("[Previous context summary]\n");
        summary_input.push_str(&previous);
        summary_input.push_str("\n\n[New conversation to incorporate]\n");
    }
    summary_input.push_str(&formatted_history);

    let summary_messages = vec![
        Message::System(SUMMARY_PROMPT.to_string()),
        Message::User(summary_input),
    ];

    // Build preferences input
    let existing_prefs = std::fs::read_to_string(&prefs_path).unwrap_or_default();
    let mut prefs_input = String::new();
    if !existing_prefs.trim().is_empty() {
        prefs_input.push_str("[Existing user preferences]\n");
        prefs_input.push_str(&existing_prefs);
        prefs_input.push_str("\n\n[New conversation to analyze]\n");
    }
    prefs_input.push_str(&formatted_history);

    let prefs_messages = vec![
        Message::System(PREFERENCES_PROMPT.to_string()),
        Message::User(prefs_input),
    ];

    // Run both LLM calls in parallel
    let provider2 = provider.clone();
    let (summary_result, prefs_result) = tokio::join!(
        provider.chat(&summary_messages, &[]),
        provider2.chat(&prefs_messages, &[]),
    );

    // Write summary and re-add recent messages
    match summary_result {
        Ok(response) => {
            if let Some(summary) = response.content {
                let _ = std::fs::create_dir_all(&summary_dir);
                if std::fs::write(&summary_path, &summary).is_ok() {
                    let _ = storage.clear(conversation_id).await;
                    // Re-add recent messages to preserve active context
                    for msg in &to_keep {
                        if let Err(e) = storage.add_message(conversation_id, msg).await {
                            log::warn!("Failed to re-add kept message: {e}");
                        }
                    }
                    log::info!(
                        "Conversation '{}' summarized: removed {} old messages, kept {}",
                        conversation_id,
                        split_point,
                        to_keep.len(),
                    );
                } else {
                    log::warn!("Failed to write summary file for '{}'", conversation_id);
                }
            }
        }
        Err(e) => {
            log::warn!("Failed to summarize '{}': {e}", conversation_id);
        }
    }

    // Write preferences
    match prefs_result {
        Ok(response) => {
            if let Some(prefs) = response.content {
                let trimmed = prefs.trim();
                if trimmed == "NO_UPDATE" || trimmed.is_empty() {
                    log::info!("No new user preferences extracted");
                } else if std::fs::write(&prefs_path, &prefs).is_ok() {
                    log::info!("User preferences updated ({} chars)", prefs.len());
                } else {
                    log::warn!("Failed to write preferences file");
                }
            }
        }
        Err(e) => {
            log::warn!("Failed to extract preferences: {e}");
        }
    }

    // Consolidate facts if they've grown large
    let facts_path = memory_dir.join("facts.md");
    if let Ok(facts_content) = std::fs::read_to_string(&facts_path) {
        if facts_content.len() > 6000 {
            log::info!("Facts file large ({} chars), consolidating...", facts_content.len());
            let consolidate_messages = vec![
                Message::System(CONSOLIDATE_FACTS_PROMPT.to_string()),
                Message::User(facts_content),
            ];
            if let Ok(response) = provider.chat(&consolidate_messages, &[]).await {
                if let Some(consolidated) = response.content {
                    let out = format!("# User Facts\n\n{}\n", consolidated.trim());
                    if std::fs::write(&facts_path, &out).is_ok() {
                        log::info!("Facts consolidated ({} chars)", consolidated.len());
                    }
                }
            }
        }
    }
}

/// Background task: extract durable facts from a user message.
async fn extract_facts(
    provider: Arc<dyn LlmProvider>,
    memory_dir: &std::path::Path,
    user_message: &str,
) {
    let facts_path = memory_dir.join("facts.md");
    let existing_facts = std::fs::read_to_string(&facts_path).unwrap_or_default();

    let mut input = String::new();
    if !existing_facts.trim().is_empty() {
        input.push_str("[Existing facts — do NOT repeat these]\n");
        input.push_str(&existing_facts);
        input.push_str("\n\n");
    }
    input.push_str("[User message to analyze]\n");
    input.push_str(user_message);

    let messages = vec![
        Message::System(EXTRACT_FACTS_PROMPT.to_string()),
        Message::User(input),
    ];

    match provider.chat(&messages, &[]).await {
        Ok(response) => {
            if let Some(facts) = response.content {
                let trimmed = facts.trim();
                if trimmed == "NO_FACTS" || trimmed.is_empty() {
                    log::debug!("Auto-extract: no new facts found");
                    return;
                }

                let timestamp = chrono::Utc::now().format("%Y-%m-%d").to_string();
                let entry = format!("\n## {timestamp}\n{trimmed}\n");

                match std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&facts_path)
                {
                    Ok(mut file) => {
                        use std::io::Write;
                        if let Err(e) = file.write_all(entry.as_bytes()) {
                            log::warn!("Auto-extract: failed to write facts: {e}");
                        } else {
                            log::info!("Auto-extract: saved new facts ({} chars)", trimmed.len());
                        }
                    }
                    Err(e) => {
                        log::warn!("Auto-extract: failed to open facts.md: {e}");
                    }
                }
            }
        }
        Err(e) => {
            log::warn!("Auto-extract: LLM call failed: {e}");
        }
    }
}

fn format_history_for_summary(messages: &[Message]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for msg in messages {
        match msg {
            Message::System(_) => {}
            Message::User(content) => {
                parts.push(format!("User: {}", truncate_for_summary(content, 500)));
            }
            Message::UserMultimodal { parts: msg_parts } => {
                let text: String = msg_parts
                    .iter()
                    .filter_map(|p| match p {
                        crate::core::ContentPart::Text(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                let label = if msg_parts.iter().any(|p| matches!(p, crate::core::ContentPart::ImageBase64 { .. })) {
                    "User [with image]"
                } else {
                    "User"
                };
                parts.push(format!("{label}: {}", truncate_for_summary(&text, 500)));
            }
            Message::Assistant {
                content,
                tool_calls,
            } => {
                if let Some(c) = content {
                    parts.push(format!("Assistant: {}", truncate_for_summary(c, 500)));
                }
                for tc in tool_calls {
                    parts.push(format!("[Used tool: {}]", tc.name));
                }
            }
            Message::ToolResult { content, .. } => {
                parts.push(format!(
                    "[Tool output: {}]",
                    truncate_for_summary(content, 150)
                ));
            }
        }
    }
    let text = parts.join("\n");
    if text.len() > 8000 {
        let end = text.floor_char_boundary(8000);
        format!("{}...\n\n[Truncated]", &text[..end])
    } else {
        text
    }
}

fn system_info() -> String {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let shell = std::env::var("SHELL").unwrap_or_default();
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();

    let sudo_note = if std::env::var("SUDO_PASSWORD").ok().filter(|p| !p.is_empty()).is_some() {
        " SUDO_PASSWORD is set — use sudo freely in shell commands, the password is piped automatically."
    } else {
        ""
    };

    format!(
        "[System: {os} ({arch}), shell: {shell}, cwd: {cwd}, home: {home}]{sudo_note}"
    )
}

fn truncate_for_summary(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let end = s.floor_char_boundary(max);
        format!("{}...", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{LlmError, LlmResponse, Message, ToolDefinition};
    use crate::storage::in_memory::InMemoryStorage;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock provider that returns responses from a predefined sequence.
    struct SequentialMockProvider {
        responses: Vec<Result<LlmResponse, LlmError>>,
        call_index: AtomicUsize,
    }

    impl SequentialMockProvider {
        fn new(responses: Vec<Result<LlmResponse, LlmError>>) -> Self {
            Self {
                responses,
                call_index: AtomicUsize::new(0),
            }
        }

        fn text(content: &str) -> Result<LlmResponse, LlmError> {
            Ok(LlmResponse {
                content: Some(content.to_string()),
                tool_calls: vec![],
                usage: None,
            })
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for SequentialMockProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolDefinition],
        ) -> Result<LlmResponse, LlmError> {
            let idx = self.call_index.fetch_add(1, Ordering::SeqCst);
            if idx < self.responses.len() {
                self.responses[idx].clone()
            } else {
                panic!("SequentialMockProvider: no more responses (call #{idx})");
            }
        }
    }

    fn make_agent(
        provider: Arc<dyn LlmProvider>,
        monitor_provider: Option<Arc<dyn LlmProvider>>,
    ) -> Agent {
        let storage: Arc<dyn crate::storage::ConversationStorage> =
            Arc::new(InMemoryStorage::new());
        let mut agent = Agent::new(provider, vec![], Some("You are helpful.".into()), storage);
        if let Some(mp) = monitor_provider {
            agent = agent.with_monitor(MonitorConfig::new(mp));
        }
        agent
    }

    #[tokio::test]
    async fn monitor_approves_response() {
        // Agent returns a response, monitor approves it
        let agent_provider = Arc::new(SequentialMockProvider::new(vec![
            SequentialMockProvider::text("Hello, world!"),
        ]));
        let monitor_provider = Arc::new(SequentialMockProvider::new(vec![
            SequentialMockProvider::text("APPROVED"),
        ]));

        let agent = make_agent(agent_provider, Some(monitor_provider));
        let result = agent.process("test-conv", "hi", None).await.unwrap();
        assert_eq!(result, "Hello, world!");
    }

    #[tokio::test]
    async fn monitor_gives_feedback_and_agent_refines() {
        // Call sequence:
        // 1. Agent: initial draft
        // 2. Monitor: feedback
        // 3. Agent: refined response
        // 4. Monitor: approved (second review won't happen since max_reviews=1)
        let agent_provider = Arc::new(SequentialMockProvider::new(vec![
            SequentialMockProvider::text("Draft response"),
            SequentialMockProvider::text("Refined response"),
        ]));
        let monitor_provider = Arc::new(SequentialMockProvider::new(vec![
            SequentialMockProvider::text("The response lacks detail. Please elaborate."),
        ]));

        let agent = make_agent(agent_provider, Some(monitor_provider));
        let result = agent.process("test-conv", "hi", None).await.unwrap();
        // After monitor feedback, agent refines — max_reviews=1 so second response goes through
        assert_eq!(result, "Refined response");
    }

    #[tokio::test]
    async fn monitor_error_approves_by_default() {
        let agent_provider = Arc::new(SequentialMockProvider::new(vec![
            SequentialMockProvider::text("My response"),
        ]));
        let monitor_provider = Arc::new(SequentialMockProvider::new(vec![
            Err(LlmError::ApiError("connection failed".into())),
        ]));

        let agent = make_agent(agent_provider, Some(monitor_provider));
        let result = agent.process("test-conv", "hi", None).await.unwrap();
        assert_eq!(result, "My response");
    }

    #[tokio::test]
    async fn no_monitor_passes_through() {
        let agent_provider = Arc::new(SequentialMockProvider::new(vec![
            SequentialMockProvider::text("Direct response"),
        ]));

        let agent = make_agent(agent_provider, None);
        let result = agent.process("test-conv", "hi", None).await.unwrap();
        assert_eq!(result, "Direct response");
    }

    #[test]
    fn trivial_replies_detected() {
        assert!(is_trivial_reply("да"));
        assert!(is_trivial_reply("ok"));
        assert!(is_trivial_reply("Yes"));
        assert!(is_trivial_reply("спасибо"));
        assert!(is_trivial_reply("👍"));
        assert!(is_trivial_reply("давай"));
        assert!(!is_trivial_reply("давай параллелить"));
        assert!(!is_trivial_reply("Я работаю в компании Acme и использую Rust"));
        assert!(!is_trivial_reply("My server IP is 192.168.1.1"));
    }

    #[tokio::test]
    async fn extract_facts_no_facts() {
        let provider = Arc::new(SequentialMockProvider::new(vec![
            SequentialMockProvider::text("NO_FACTS"),
        ]));
        let dir = tempfile::tempdir().unwrap();
        extract_facts(provider, dir.path(), "fix the bug on line 42").await;
        // facts.md should not exist
        assert!(!dir.path().join("facts.md").exists());
    }

    #[tokio::test]
    async fn extract_facts_writes_file() {
        let provider = Arc::new(SequentialMockProvider::new(vec![
            SequentialMockProvider::text("- User works at Acme Corp\n- User uses Rust"),
        ]));
        let dir = tempfile::tempdir().unwrap();
        extract_facts(provider, dir.path(), "I work at Acme Corp and I use Rust").await;

        let content = std::fs::read_to_string(dir.path().join("facts.md")).unwrap();
        assert!(content.contains("Acme Corp"));
        assert!(content.contains("Rust"));
    }

    #[tokio::test]
    async fn extract_facts_appends() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("facts.md"), "## 2026-03-15\n- Existing fact\n").unwrap();

        let provider = Arc::new(SequentialMockProvider::new(vec![
            SequentialMockProvider::text("- New fact about user"),
        ]));
        extract_facts(provider, dir.path(), "something new").await;

        let content = std::fs::read_to_string(dir.path().join("facts.md")).unwrap();
        assert!(content.contains("Existing fact"));
        assert!(content.contains("New fact about user"));
    }

    #[tokio::test]
    async fn spawn_extract_respects_rate_limit() {
        let provider = Arc::new(SequentialMockProvider::new(vec![
            // Provide one response for the first extract call
            SequentialMockProvider::text("NO_FACTS"),
        ]));
        let storage: Arc<dyn crate::storage::ConversationStorage> =
            Arc::new(InMemoryStorage::new());
        let dir = tempfile::tempdir().unwrap();

        let agent = Agent::new(provider, vec![], None, storage)
            .with_auto_extract(true)
            .with_memory_dir(dir.path().to_path_buf());

        // First call should update last_extract_time
        agent.spawn_extract("This is a long enough message to pass filter");

        // Verify the time was updated recently
        let last = agent.last_extract_time.lock().unwrap();
        let elapsed = last.elapsed();
        assert!(elapsed < std::time::Duration::from_secs(1));
    }

    #[tokio::test]
    async fn spawn_extract_skips_trivial() {
        let provider = Arc::new(SequentialMockProvider::new(vec![]));
        let storage: Arc<dyn crate::storage::ConversationStorage> =
            Arc::new(InMemoryStorage::new());
        let dir = tempfile::tempdir().unwrap();

        let agent = Agent::new(provider, vec![], None, storage)
            .with_auto_extract(true)
            .with_memory_dir(dir.path().to_path_buf());

        // Short message — should be skipped (no LLM call, so no panic)
        agent.spawn_extract("ok");
        // Command — should be skipped
        agent.spawn_extract("/help");
    }

    #[test]
    fn load_facts_truncates_large_content() {
        let dir = tempfile::tempdir().unwrap();
        let large = "x".repeat(5000);
        std::fs::write(dir.path().join("facts.md"), &large).unwrap();

        let storage: Arc<dyn crate::storage::ConversationStorage> =
            Arc::new(InMemoryStorage::new());
        let provider = Arc::new(SequentialMockProvider::new(vec![]));
        let agent = Agent::new(provider, vec![], None, storage)
            .with_memory_dir(dir.path().to_path_buf());

        let facts = agent.load_facts().unwrap();
        assert!(facts.len() <= 4000);
    }
}
