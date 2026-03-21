pub mod agent;
pub mod anthropic;
pub mod approval;
pub mod chatgpt;
pub mod config;
pub mod debug_provider;
pub mod langfuse;
pub mod openai;
pub mod provider_manager;
pub mod tool_selector;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Error)]
pub enum LlmError {
    #[error("request build error: {0}")]
    RequestBuildError(String),
    #[error("API error: {0}")]
    ApiError(String),
    #[error("empty response")]
    EmptyResponse,
    #[error("storage error: {0}")]
    StorageError(String),
    #[error("approval error: {0}")]
    ApprovalError(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallInfo {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Message {
    System(String),
    User(String),
    Assistant {
        content: Option<String>,
        tool_calls: Vec<ToolCallInfo>,
    },
    ToolResult {
        tool_call_id: String,
        content: String,
    },
}

#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Debug, Clone)]
pub struct LlmResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCallInfo>,
    pub usage: Option<TokenUsage>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields read via pattern matching in CLI connector
pub enum StreamEvent {
    Token(String),
    ContentDone(String),
    ToolCallStart {
        id: String,
        name: String,
        arguments: String,
    },
    ToolCallResult {
        id: String,
        name: String,
        result: String,
        is_error: bool,
    },
    IterationStart(usize),
    Usage {
        input_tokens: u32,
        output_tokens: u32,
        message_count: usize,
        summary_threshold: usize,
    },
    TaskSpawned {
        id: String,
        description: String,
    },
    TaskUpdate {
        id: String,
        status: String,
    },
    // -- Workflow engine events --
    WorkflowAssessment {
        score: u8,
        reasoning: String,
    },
    WorkflowPlanReady {
        node_count: usize,
        edge_count: usize,
    },
    WorkflowNodeStart {
        node_id: String,
        description: String,
    },
    WorkflowNodeComplete {
        node_id: String,
        success: bool,
    },
    WorkflowProgress {
        completed: usize,
        total: usize,
    },
    WorkflowTraceReady {
        summary: String,
        trace_path: Option<String>,
    },
    Done(String),
    Error(String),
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<LlmResponse, LlmError>;

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        tx: mpsc::UnboundedSender<StreamEvent>,
    ) -> Result<LlmResponse, LlmError> {
        let response = self.chat(messages, tools).await?;
        if let Some(ref content) = response.content {
            let _ = tx.send(StreamEvent::Token(content.clone()));
        }
        Ok(response)
    }
}
