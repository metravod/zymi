use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;

use tokio::sync::oneshot;
use tui_textarea::TextArea;

use crate::core::debug_provider::DebugEvent;
use crate::core::StreamEvent;
use crate::events::{Event, EventKind};

pub const PROVIDER_OPTIONS: &[&str] = &["OpenAI Compatible", "Anthropic", "ChatGPT Plus (OAuth)"];

#[derive(Clone)]
pub enum AddModelStep {
    Provider,
    ModelId,
    DisplayName,
    BaseUrl,
    ApiKey,
    EnvVarName,
}

#[derive(Clone)]
pub struct AddModelForm {
    pub step: AddModelStep,
    pub provider_index: usize,
    pub model_id: String,
    pub display_name: String,
    pub base_url: String,
    pub api_key: String,
    pub env_var_name: String,
    pub input_buffer: String,
}

impl AddModelForm {
    pub fn new() -> Self {
        Self {
            step: AddModelStep::Provider,
            provider_index: 0,
            model_id: String::new(),
            display_name: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            env_var_name: String::new(),
            input_buffer: String::new(),
        }
    }
}

pub struct UsageStats {
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub message_count: usize,
    pub summary_threshold: usize,
}

impl Default for UsageStats {
    fn default() -> Self {
        Self {
            total_input_tokens: 0,
            total_output_tokens: 0,
            message_count: 0,
            summary_threshold: 50,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum LeftPanelSection {
    Models,
    SystemFiles,
    SubAgents,
}

impl LeftPanelSection {
    pub fn next(&self) -> Self {
        match self {
            LeftPanelSection::Models => LeftPanelSection::SystemFiles,
            LeftPanelSection::SystemFiles => LeftPanelSection::SubAgents,
            LeftPanelSection::SubAgents => LeftPanelSection::Models,
        }
    }

    pub fn prev(&self) -> Self {
        match self {
            LeftPanelSection::Models => LeftPanelSection::SubAgents,
            LeftPanelSection::SystemFiles => LeftPanelSection::Models,
            LeftPanelSection::SubAgents => LeftPanelSection::SystemFiles,
        }
    }
}

pub struct ObservabilityEntry {
    pub timestamp: String,
    pub icon: &'static str,
    pub kind: String,
    pub detail: String,
    pub full_detail: String, // untruncated for expand view
}

const MAX_OBSERVABILITY_ENTRIES: usize = 500;

pub struct App {
    pub messages: Vec<ChatEntry>,
    pub input: TextArea<'static>,
    pub scroll_offset: u16,
    pub is_processing: bool,
    pub spinner_frame: usize,
    pub pending_approval: Option<PendingApproval>,
    pub approval_selected: bool, // true = Yes, false = No
    pub pending_question: Option<PendingQuestion>,
    pub should_quit: bool,
    pub conversation_id: String,
    pub auto_scroll: bool,
    pub total_content_height: u16,
    pub visible_height: u16,
    pub model_selector_open: bool,
    pub model_selector_index: usize,
    pub available_models: Vec<ModelSelectorEntry>,
    pub current_model_id: String,
    pub add_model_form: Option<AddModelForm>,
    pub copy_mode: bool,
    pub debug_mode: bool,
    pub input_width: u16,
    pub usage: UsageStats,
    pub input_price_per_1m: Option<f64>,
    pub output_price_per_1m: Option<f64>,
    // -- Panel state --
    pub left_panel_visible: bool,
    pub right_panel_visible: bool,
    pub left_panel_focused: bool,
    pub left_panel_section: LeftPanelSection,
    pub left_panel_index: usize,
    pub right_panel_events: VecDeque<ObservabilityEntry>,
    pub right_panel_scroll: u16,
    pub right_panel_auto_scroll: bool,
    pub right_panel_total_lines: u16,
    pub right_panel_visible_height: u16,
    pub right_panel_x_range: (u16, u16), // (x_start, x_end) for mouse hit-test
    pub right_panel_focused: bool,
    pub right_panel_selected: usize,
    pub right_panel_expanded: HashSet<usize>, // indices of expanded events
    pub system_files: Vec<String>,
    pub subagent_files: Vec<String>,
    pub memory_dir: PathBuf,
}

#[derive(Clone)]
pub struct ModelSelectorEntry {
    pub id: String,
    pub name: String,
}

pub enum ChatEntry {
    UserMessage(String),
    AssistantChunk {
        content: String,
        is_complete: bool,
    },
    ToolCall {
        name: String,
        arguments: String,
        result: Option<String>,
        is_error: bool,
        is_running: bool,
    },
    SystemMessage(String),
    DebugMessage {
        caller: String,
        content: String,
    },
}

pub struct PendingApproval {
    pub tool_description: String,
    pub explanation: Option<String>,
    pub responder: oneshot::Sender<bool>,
}

pub struct PendingQuestion {
    pub responder: oneshot::Sender<String>,
}

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

impl App {
    pub fn new(
        conversation_id: String,
        available_models: Vec<ModelSelectorEntry>,
        current_model_id: String,
        debug_mode: bool,
        input_price_per_1m: Option<f64>,
        output_price_per_1m: Option<f64>,
        memory_dir: PathBuf,
    ) -> Self {
        let mut input = TextArea::default();
        input.set_cursor_line_style(ratatui::style::Style::default());
        input.set_placeholder_text("Type your message... (Enter to send, Q to quit)");

        // Scan for system files and subagents
        let system_files = vec!["AGENT.md".to_string()];
        let subagent_dir = memory_dir.join("subagents");
        let subagent_files = std::fs::read_dir(&subagent_dir)
            .ok()
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
                    .filter_map(|e| e.file_name().into_string().ok())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        Self {
            messages: Vec::new(),
            input,
            scroll_offset: 0,
            is_processing: false,
            spinner_frame: 0,
            pending_approval: None,
            approval_selected: true,
            pending_question: None,
            should_quit: false,
            conversation_id,
            auto_scroll: true,
            total_content_height: 0,
            visible_height: 0,
            model_selector_open: false,
            model_selector_index: 0,
            available_models,
            current_model_id,
            add_model_form: None,
            copy_mode: false,
            debug_mode,
            input_width: 80,
            usage: UsageStats::default(),
            input_price_per_1m,
            output_price_per_1m,
            left_panel_visible: true,
            right_panel_visible: true,
            left_panel_focused: false,
            left_panel_section: LeftPanelSection::Models,
            left_panel_index: 0,
            right_panel_events: VecDeque::new(),
            right_panel_scroll: 0,
            right_panel_auto_scroll: true,
            right_panel_total_lines: 0,
            right_panel_visible_height: 0,
            right_panel_x_range: (0, 0),
            right_panel_focused: false,
            right_panel_selected: 0,
            right_panel_expanded: HashSet::new(),
            system_files,
            subagent_files,
            memory_dir,
        }
    }

    pub fn spinner(&self) -> &str {
        SPINNER_FRAMES[self.spinner_frame % SPINNER_FRAMES.len()]
    }

    pub fn tick_spinner(&mut self) {
        self.spinner_frame = self.spinner_frame.wrapping_add(1);
    }

    pub fn handle_stream_event(&mut self, event: StreamEvent) {
        match event {
            StreamEvent::Token(text) => {
                // Append to the last AssistantChunk or create a new one
                if let Some(ChatEntry::AssistantChunk {
                    ref mut content,
                    is_complete: false,
                    ..
                }) = self.messages.last_mut()
                {
                    content.push_str(&text);
                } else {
                    self.messages.push(ChatEntry::AssistantChunk {
                        content: text,
                        is_complete: false,
                    });
                }
                if self.auto_scroll {
                    self.scroll_to_bottom();
                }
            }
            StreamEvent::ContentDone(_) => {
                if let Some(ChatEntry::AssistantChunk {
                    ref mut is_complete,
                    ..
                }) = self.messages.last_mut()
                {
                    *is_complete = true;
                }
            }
            StreamEvent::ToolCallStart {
                name, arguments, ..
            } => {
                self.messages.push(ChatEntry::ToolCall {
                    name,
                    arguments,
                    result: None,
                    is_error: false,
                    is_running: true,
                });
                if self.auto_scroll {
                    self.scroll_to_bottom();
                }
            }
            StreamEvent::ToolCallResult {
                name,
                result,
                is_error,
                ..
            } => {
                // Find the matching running tool call and update it
                for entry in self.messages.iter_mut().rev() {
                    if let ChatEntry::ToolCall {
                        name: ref tc_name,
                        is_running: ref mut running,
                        result: ref mut tc_result,
                        is_error: ref mut tc_error,
                        ..
                    } = entry
                    {
                        if tc_name == &name && *running {
                            *running = false;
                            *tc_result = Some(result.clone());
                            *tc_error = is_error;
                            break;
                        }
                    }
                }
                if self.auto_scroll {
                    self.scroll_to_bottom();
                }
            }
            StreamEvent::IterationStart(_) => {}
            StreamEvent::TaskSpawned { description, .. } => {
                self.messages.push(ChatEntry::SystemMessage(
                    format!("Task spawned: {description}"),
                ));
            }
            StreamEvent::TaskUpdate { id, status } => {
                self.messages.push(ChatEntry::SystemMessage(
                    format!("Task {} — {status}", &id[..id.len().min(8)]),
                ));
            }
            StreamEvent::Usage {
                input_tokens,
                output_tokens,
                message_count,
                summary_threshold,
            } => {
                self.usage.total_input_tokens += input_tokens as u64;
                self.usage.total_output_tokens += output_tokens as u64;
                self.usage.message_count = message_count;
                self.usage.summary_threshold = summary_threshold;
            }
            // -- Workflow events --
            StreamEvent::WorkflowAssessment { score, reasoning } => {
                self.messages.push(ChatEntry::SystemMessage(format!(
                    "Workflow: complexity {score}/10 — {reasoning}"
                )));
                if self.auto_scroll {
                    self.scroll_to_bottom();
                }
            }
            StreamEvent::WorkflowPlanReady {
                node_count,
                edge_count,
            } => {
                self.messages.push(ChatEntry::SystemMessage(format!(
                    "Workflow: plan ready — {node_count} steps, {edge_count} dependencies"
                )));
                if self.auto_scroll {
                    self.scroll_to_bottom();
                }
            }
            StreamEvent::WorkflowNodeStart {
                node_id,
                description,
            } => {
                self.messages.push(ChatEntry::SystemMessage(format!(
                    "Workflow [{node_id}]: {description}"
                )));
                if self.auto_scroll {
                    self.scroll_to_bottom();
                }
            }
            StreamEvent::WorkflowNodeComplete { node_id, success } => {
                let status = if success { "done" } else { "FAILED" };
                self.messages.push(ChatEntry::SystemMessage(format!(
                    "Workflow [{node_id}]: {status}"
                )));
                if self.auto_scroll {
                    self.scroll_to_bottom();
                }
            }
            StreamEvent::WorkflowProgress { completed, total } => {
                log::debug!("Workflow progress: {completed}/{total}");
            }
            StreamEvent::WorkflowTraceReady {
                summary,
                trace_path,
            } => {
                let path_info = trace_path
                    .map(|p| format!("\nTrace saved: {p}"))
                    .unwrap_or_default();
                self.messages.push(ChatEntry::SystemMessage(format!(
                    "{summary}{path_info}"
                )));
                if self.auto_scroll {
                    self.scroll_to_bottom();
                }
            }
            StreamEvent::Done(ref msg) => {
                if !self.is_processing && !msg.is_empty() {
                    // Background event (e.g. OAuth login complete)
                    self.messages.push(ChatEntry::SystemMessage(msg.clone()));
                    if self.auto_scroll {
                        self.scroll_to_bottom();
                    }
                }
                self.is_processing = false;
            }
            StreamEvent::Error(err) => {
                self.is_processing = false;
                self.messages.push(ChatEntry::SystemMessage(format!(
                    "Error: {}",
                    err
                )));
            }
        }
    }

    pub fn scroll_up(&mut self, amount: u16) {
        self.auto_scroll = false;
        self.scroll_offset = self.scroll_offset.saturating_add(amount);
        let max_scroll = self.total_content_height.saturating_sub(self.visible_height);
        if self.scroll_offset > max_scroll {
            self.scroll_offset = max_scroll;
        }
    }

    pub fn scroll_down(&mut self, amount: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(amount);
        if self.scroll_offset == 0 {
            self.auto_scroll = true;
        }
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
        self.auto_scroll = true;
    }

    pub fn handle_debug_event(&mut self, event: DebugEvent) {
        let mut parts = Vec::new();
        if let Some(ref text) = event.content {
            parts.push(text.clone());
        }
        for tc in &event.tool_calls {
            parts.push(format!("[tool] {tc}"));
        }
        let content = if parts.is_empty() {
            "(empty response)".to_string()
        } else {
            parts.join("\n")
        };

        self.messages.push(ChatEntry::DebugMessage {
            caller: event.caller,
            content,
        });
        if self.auto_scroll {
            self.scroll_to_bottom();
        }
    }

    pub fn handle_approval(&mut self, approved: bool) {
        if let Some(pending) = self.pending_approval.take() {
            let _ = pending.responder.send(approved);
            let status = if approved { "Approved" } else { "Rejected" };
            self.messages
                .push(ChatEntry::SystemMessage(format!("{}: {}", status, pending.tool_description)));
        }
    }

    pub fn handle_question_response(&mut self, response: String) {
        if let Some(pending) = self.pending_question.take() {
            self.messages.push(ChatEntry::UserMessage(response.clone()));
            let _ = pending.responder.send(response);
            if self.auto_scroll {
                self.scroll_to_bottom();
            }
        }
    }

    pub fn get_input_and_clear(&mut self) -> String {
        let lines: Vec<String> = self.input.lines().iter().map(|s| s.to_string()).collect();
        let text = lines.join("\n");
        self.input = TextArea::default();
        self.input.set_cursor_line_style(ratatui::style::Style::default());
        self.input
            .set_placeholder_text("Type your message... (Enter to send, Q to quit)");
        text
    }

    pub fn handle_domain_event(&mut self, event: Event) {
        let ts = event.timestamp.format("%H:%M:%S").to_string();
        let (icon, kind, detail, full_detail) = match &event.kind {
            EventKind::AgentProcessingStarted { conversation_id } => (
                "▶", "Processing".into(), String::new(), format!("conversation: {conversation_id}"),
            ),
            EventKind::AgentProcessingCompleted { success, conversation_id } => (
                if *success { "✓" } else { "✗" },
                "Completed".into(),
                if *success { String::new() } else { "with errors".into() },
                format!("conversation: {conversation_id}, success: {success}"),
            ),
            EventKind::LlmCallStarted { iteration, message_count, approx_context_chars } => {
                let ctx_k = *approx_context_chars / 1024;
                (
                    "⚡", "LLM call".into(),
                    format!("iter {} | {}msg ~{}k", iteration + 1, message_count, ctx_k),
                    format!("iteration: {}\nmessages: {}\napprox context: {} chars (~{}k)", iteration + 1, message_count, approx_context_chars, ctx_k),
                )
            }
            EventKind::LlmCallCompleted { has_tool_calls, usage, content_preview } => {
                let tokens = usage.as_ref()
                    .map(|u| format!("↑{} ↓{}", u.input_tokens, u.output_tokens))
                    .unwrap_or_default();
                let tools = if *has_tool_calls { " +tools" } else { "" };
                let preview = content_preview.as_deref().unwrap_or("");
                let preview_short = if preview.len() > 60 {
                    format!("{}...", &preview[..preview.floor_char_boundary(60)])
                } else {
                    preview.to_string()
                };
                let mut full = usage.as_ref()
                    .map(|u| format!("input: {} tokens, output: {} tokens\ntool_calls: {}", u.input_tokens, u.output_tokens, has_tool_calls))
                    .unwrap_or_default();
                if !preview.is_empty() {
                    full.push_str(&format!("\nresponse: {preview}"));
                }
                ("✓", "LLM done".into(), format!("{tokens}{tools} {preview_short}"), full)
            }
            EventKind::ToolCallRequested { tool_name, arguments, call_id } => {
                let args_short = if arguments.len() > 60 {
                    format!("{}...", &arguments[..arguments.floor_char_boundary(60)])
                } else {
                    arguments.clone()
                };
                let full = format!("call_id: {call_id}\ntool: {tool_name}\nargs: {arguments}");
                ("🔧", format!("Tool: {tool_name}"), args_short, full)
            }
            EventKind::ToolCallCompleted { call_id, result_preview, is_error, duration_ms } => {
                let icon = if *is_error { "✗" } else { "✓" };
                let preview = if result_preview.len() > 60 {
                    format!("{}...", &result_preview[..result_preview.floor_char_boundary(60)])
                } else {
                    result_preview.clone()
                };
                let full = format!("call_id: {call_id}\nduration: {duration_ms}ms\nerror: {is_error}\nresult: {result_preview}");
                (icon, "Tool done".into(), format!("{preview} ({duration_ms}ms)"), full)
            }
            EventKind::IntentionEmitted { intention_tag, intention_data } => (
                "📋", "Intention".into(), intention_tag.clone(),
                format!("tag: {intention_tag}\ndata: {intention_data}"),
            ),
            EventKind::IntentionEvaluated { intention_tag, verdict } => (
                "📝", "Contract".into(), format!("{intention_tag}: {verdict}"),
                format!("tag: {intention_tag}\nverdict: {verdict}"),
            ),
            EventKind::ApprovalRequested { description, approval_id } => (
                "⚠", "Approval".into(), description.clone(),
                format!("id: {approval_id}\n{description}"),
            ),
            EventKind::ApprovalDecided { approved, approval_id } => (
                if *approved { "✓" } else { "✗" },
                "Decision".into(),
                if *approved { "approved".into() } else { "rejected".into() },
                format!("id: {approval_id}, approved: {approved}"),
            ),
            EventKind::ResponseReady { conversation_id, content } => {
                let preview = if content.len() > 80 {
                    format!("{}...", &content[..content.floor_char_boundary(80)])
                } else {
                    content.clone()
                };
                ("📨", "Response".into(), preview, format!("conversation: {conversation_id}\n{content}"))
            }
            EventKind::WorkflowStarted { node_count, user_message } => (
                "🔀", "Workflow".into(), format!("{node_count} nodes"),
                format!("nodes: {node_count}\nmessage: {user_message}"),
            ),
            EventKind::WorkflowNodeStarted { node_id, description } => (
                "▶", format!("WF:{node_id}"), description.clone(),
                format!("node: {node_id}\n{description}"),
            ),
            EventKind::WorkflowNodeCompleted { node_id, success } => (
                if *success { "✓" } else { "✗" },
                format!("WF:{node_id}"), String::new(),
                format!("node: {node_id}, success: {success}"),
            ),
            EventKind::WorkflowCompleted { success } => (
                if *success { "✓" } else { "✗" },
                "WF done".into(), String::new(),
                format!("success: {success}"),
            ),
            _ => return, // Skip UserMessageReceived, ScheduledTaskTriggered
        };

        self.right_panel_events.push_back(ObservabilityEntry {
            timestamp: ts,
            icon,
            kind,
            detail,
            full_detail,
        });

        // Cap at MAX_OBSERVABILITY_ENTRIES
        while self.right_panel_events.len() > MAX_OBSERVABILITY_ENTRIES {
            self.right_panel_events.pop_front();
        }

        // Reset scroll to bottom on new events (if user hasn't scrolled up)
        if self.right_panel_auto_scroll {
            self.right_panel_scroll = 0;
        }
    }

    /// Number of items in the currently focused left panel section.
    pub fn left_panel_section_len(&self) -> usize {
        match self.left_panel_section {
            LeftPanelSection::Models => self.available_models.len(),
            LeftPanelSection::SystemFiles => self.system_files.len(),
            LeftPanelSection::SubAgents => self.subagent_files.len(),
        }
    }

    /// Get the file path for the selected item in the left panel (for opening in editor).
    pub fn left_panel_selected_path(&self) -> Option<PathBuf> {
        match self.left_panel_section {
            LeftPanelSection::Models => None, // Models are switched, not opened
            LeftPanelSection::SystemFiles => {
                self.system_files.get(self.left_panel_index)
                    .map(|f| self.memory_dir.join(f))
            }
            LeftPanelSection::SubAgents => {
                self.subagent_files.get(self.left_panel_index)
                    .map(|f| self.memory_dir.join("subagents").join(f))
            }
        }
    }

    pub fn handle_command(&mut self, cmd: &str) -> bool {
        match cmd.trim() {
            "/quit" => {
                self.should_quit = true;
                true
            }
            "/clear" => {
                self.messages.clear();
                self.scroll_offset = 0;
                self.usage = UsageStats::default();
                true
            }
            _ => false,
        }
    }
}
