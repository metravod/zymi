use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;

use super::{LlmError, LlmProvider, LlmResponse, Message, StreamEvent, ToolDefinition};

#[derive(Debug, Clone)]
pub struct DebugEvent {
    pub caller: String,
    pub content: Option<String>,
    pub tool_calls: Vec<String>,
}

pub struct DebugProvider {
    inner: Arc<dyn LlmProvider>,
    debug_tx: mpsc::UnboundedSender<DebugEvent>,
}

impl DebugProvider {
    pub fn new(
        inner: Arc<dyn LlmProvider>,
        debug_tx: mpsc::UnboundedSender<DebugEvent>,
    ) -> Self {
        Self { inner, debug_tx }
    }

    fn infer_caller(messages: &[Message]) -> String {
        // Look at the system prompt to identify the caller
        if let Some(Message::System(prompt)) = messages.first() {
            let lower = prompt.to_lowercase();
            if lower.contains("monitor") {
                return "Monitor".to_string();
            }
            if lower.contains("feasibility assessor") {
                return "Simulation".to_string();
            }
            if lower.contains("sub-agent") || lower.contains("subagent") {
                return "Sub-agent".to_string();
            }
        }
        "Agent".to_string()
    }

    fn emit(&self, messages: &[Message], response: &LlmResponse) {
        let caller = Self::infer_caller(messages);
        let tool_calls = response
            .tool_calls
            .iter()
            .map(|tc| format!("{}({})", tc.name, truncate(&tc.arguments, 100)))
            .collect();

        let _ = self.debug_tx.send(DebugEvent {
            caller,
            content: response.content.clone(),
            tool_calls,
        });
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let end = s.floor_char_boundary(max);
        format!("{}...", &s[..end])
    }
}

#[async_trait]
impl LlmProvider for DebugProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<LlmResponse, LlmError> {
        let response = self.inner.chat(messages, tools).await?;
        self.emit(messages, &response);
        Ok(response)
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        tx: mpsc::UnboundedSender<StreamEvent>,
    ) -> Result<LlmResponse, LlmError> {
        let response = self.inner.chat_stream(messages, tools, tx).await?;
        self.emit(messages, &response);
        Ok(response)
    }
}
