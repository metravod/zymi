use tokio::sync::oneshot;
use tui_textarea::TextArea;

use crate::core::debug_provider::DebugEvent;
use crate::core::StreamEvent;

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
    ) -> Self {
        let mut input = TextArea::default();
        input.set_cursor_line_style(ratatui::style::Style::default());
        input.set_placeholder_text("Type your message... (Enter to send, Esc to quit)");

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
            .set_placeholder_text("Type your message... (Enter to send, Esc to quit)");
        text
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
