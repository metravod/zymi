use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, RwLock};

use crate::auth;
use crate::auth::login;
use crate::auth::storage::AuthTokens;

use super::{LlmError, LlmProvider, LlmResponse, Message, StreamEvent, TokenUsage, ToolCallInfo, ToolDefinition};

/// Provider that uses ChatGPT Plus/Pro subscription via OAuth.
/// Talks to the Responses API at chatgpt.com/backend-api/codex.
pub struct ChatgptProvider {
    client: reqwest::Client,
    model: String,
    memory_dir: PathBuf,
    tokens: RwLock<AuthTokens>,
}

impl ChatgptProvider {
    pub fn new(model: &str, memory_dir: &Path, tokens: AuthTokens) -> Self {
        Self {
            client: reqwest::Client::new(),
            model: model.to_string(),
            memory_dir: memory_dir.to_path_buf(),
            tokens: RwLock::new(tokens),
        }
    }

    /// Get the current access token, refreshing if needed.
    async fn get_access_token(&self) -> Result<String, LlmError> {
        let tokens = self.tokens.read().await;

        if !login::needs_refresh(&tokens) {
            return Ok(tokens.access_token.clone());
        }
        drop(tokens);

        // Need to refresh
        let current = self.tokens.read().await.clone();
        match login::refresh_token(&self.memory_dir, &current).await {
            Ok(new_tokens) => {
                let token = new_tokens.access_token.clone();
                *self.tokens.write().await = new_tokens;
                Ok(token)
            }
            Err(e) => {
                log::error!("Token refresh failed: {e}");
                // Return old token, it might still work
                Ok(current.access_token)
            }
        }
    }

    /// Retry with refreshed token on 401.
    async fn refresh_and_retry_token(&self) -> Result<String, LlmError> {
        let current = self.tokens.read().await.clone();
        let new_tokens = login::refresh_token(&self.memory_dir, &current)
            .await
            .map_err(|e| LlmError::ApiError(format!("Token refresh failed: {e}")))?;
        let token = new_tokens.access_token.clone();
        *self.tokens.write().await = new_tokens;
        Ok(token)
    }

    fn responses_url(&self) -> String {
        format!("{}/responses", auth::CHATGPT_API_BASE)
    }
}

// ============================================================
// Responses API types
// ============================================================

#[derive(Serialize)]
struct ResponsesRequest {
    model: String,
    instructions: String,
    input: Vec<ResponsesInput>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ResponsesTool>,
    store: bool,
    stream: bool,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ResponsesInput {
    Message {
        role: String,
        content: String,
    },
    FunctionCall {
        #[serde(rename = "type")]
        type_: String,
        name: String,
        arguments: String,
        call_id: String,
    },
    FunctionCallOutput {
        #[serde(rename = "type")]
        type_: String,
        call_id: String,
        output: String,
    },
}

#[derive(Serialize)]
struct ResponsesTool {
    #[serde(rename = "type")]
    type_: String,
    name: String,
    description: String,
    parameters: serde_json::Value,
}

// -- Streaming SSE events --

#[derive(Deserialize)]
struct SseEvent {
    #[serde(rename = "type")]
    type_: String,
    #[serde(default)]
    delta: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    text: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    response: Option<serde_json::Value>,
    /// Present on `response.output_item.added` — contains function_call metadata.
    #[serde(default)]
    item: Option<SseOutputItem>,
}

#[derive(Deserialize)]
struct SseOutputItem {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    call_id: Option<String>,
}

// ============================================================
// Convert our Message types to Responses API input
// ============================================================

fn convert_messages(messages: &[Message]) -> Vec<ResponsesInput> {
    let mut input = Vec::new();

    for msg in messages {
        match msg {
            Message::System(_) => {
                // System messages go into `instructions`, not `input`
            }
            Message::User(text) => {
                input.push(ResponsesInput::Message {
                    role: "user".to_string(),
                    content: text.clone(),
                });
            }
            Message::UserMultimodal { parts } => {
                let text: String = parts
                    .iter()
                    .filter_map(|p| match p {
                        super::ContentPart::Text(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let text = if text.is_empty() {
                    "[Image received but this model does not support vision]".to_string()
                } else {
                    text
                };
                input.push(ResponsesInput::Message {
                    role: "user".to_string(),
                    content: text,
                });
            }
            Message::Assistant {
                content,
                tool_calls,
            } => {
                if let Some(text) = content {
                    input.push(ResponsesInput::Message {
                        role: "assistant".to_string(),
                        content: text.clone(),
                    });
                }
                for tc in tool_calls {
                    input.push(ResponsesInput::FunctionCall {
                        type_: "function_call".to_string(),
                        name: tc.name.clone(),
                        arguments: tc.arguments.clone(),
                        call_id: tc.id.clone(),
                    });
                }
            }
            Message::ToolResult {
                tool_call_id,
                content,
            } => {
                input.push(ResponsesInput::FunctionCallOutput {
                    type_: "function_call_output".to_string(),
                    call_id: tool_call_id.clone(),
                    output: content.clone(),
                });
            }
        }
    }

    input
}

fn convert_tools(tools: &[ToolDefinition]) -> Vec<ResponsesTool> {
    tools
        .iter()
        .map(|t| ResponsesTool {
            type_: "function".to_string(),
            name: t.name.clone(),
            description: t.description.clone(),
            parameters: t.parameters.clone(),
        })
        .collect()
}

fn extract_instructions(messages: &[Message]) -> String {
    messages
        .iter()
        .filter_map(|m| match m {
            Message::System(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn build_request(model: &str, messages: &[Message], tools: &[ToolDefinition], stream: bool) -> ResponsesRequest {
    let instructions = extract_instructions(messages);
    ResponsesRequest {
        model: model.to_string(),
        instructions: if instructions.is_empty() {
            "You are a helpful assistant.".to_string()
        } else {
            instructions
        },
        input: convert_messages(messages),
        tools: convert_tools(tools),
        store: false,
        stream,
    }
}

// ============================================================
// LlmProvider implementation
// ============================================================

impl ChatgptProvider {
    /// Send a streaming request to the Codex endpoint and process the SSE response.
    /// The Codex endpoint always requires `stream: true`.
    /// If `tx` is provided, stream events are forwarded to it (for chat_stream).
    /// If `tx` is None, the stream is consumed silently (for chat).
    async fn send_streaming_request(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        tx: Option<&mpsc::UnboundedSender<StreamEvent>>,
    ) -> Result<LlmResponse, LlmError> {
        let request_body = build_request(&self.model, messages, tools, true);
        let access_token = self.get_access_token().await?;

        let resp = self
            .client
            .post(self.responses_url())
            .bearer_auth(&access_token)
            .header("Accept", "text/event-stream")
            .json(&request_body)
            .send()
            .await
            .map_err(|e| LlmError::ApiError(e.to_string()))?;

        // Handle 401 with token refresh
        let resp = if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            log::warn!("ChatGPT API returned 401, refreshing token");
            let new_token = self.refresh_and_retry_token().await?;
            let request_body = build_request(&self.model, messages, tools, true);
            self.client
                .post(self.responses_url())
                .bearer_auth(&new_token)
                .header("Accept", "text/event-stream")
                .json(&request_body)
                .send()
                .await
                .map_err(|e| LlmError::ApiError(e.to_string()))?
        } else {
            resp
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(LlmError::ApiError(format!("ChatGPT API error ({status}): {body}")));
        }

        // Process SSE stream
        let mut content = String::new();
        let mut tool_calls: Vec<ToolCallInfo> = Vec::new();
        let mut usage: Option<TokenUsage> = None;
        let mut fn_args_accum = String::new();
        // Track current function call metadata from output_item.added
        let mut current_fn_name = String::new();
        let mut current_fn_call_id = String::new();
        let mut line_buf = String::new();
        let mut stream = resp.bytes_stream();
        let mut done = false;

        while let Some(chunk_result) = futures::StreamExt::next(&mut stream).await {
            let chunk = chunk_result.map_err(|e| LlmError::ApiError(e.to_string()))?;
            let chunk_str = String::from_utf8_lossy(&chunk);
            line_buf.push_str(&chunk_str);

            while let Some(newline_pos) = line_buf.find('\n') {
                let line = line_buf[..newline_pos].trim().to_string();
                line_buf = line_buf[newline_pos + 1..].to_string();

                if !line.starts_with("data: ") {
                    continue;
                }
                let data = &line[6..];
                if data == "[DONE]" {
                    done = true;
                    break;
                }

                let event: SseEvent = match serde_json::from_str(data) {
                    Ok(e) => e,
                    Err(e) => {
                        log::debug!("Failed to parse SSE event: {e}, data: {data}");
                        continue;
                    }
                };

                match event.type_.as_str() {
                    // Capture function call metadata (name, call_id) when the item is first added
                    "response.output_item.added" => {
                        if let Some(ref item) = event.item {
                            if let Some(ref name) = item.name {
                                current_fn_name = name.clone();
                            }
                            if let Some(ref call_id) = item.call_id {
                                current_fn_call_id = call_id.clone();
                            }
                        }
                    }
                    "response.output_text.delta" => {
                        if let Some(ref delta) = event.delta {
                            content.push_str(delta);
                            if let Some(tx) = tx {
                                let _ = tx.send(StreamEvent::Token(delta.clone()));
                            }
                        }
                    }
                    "response.output_text.done" => {}
                    "response.function_call_arguments.delta" => {
                        if let Some(ref delta) = event.delta {
                            fn_args_accum.push_str(delta);
                        }
                    }
                    "response.function_call_arguments.done" => {
                        // Use metadata from output_item.added, fall back to event fields
                        let call_id = event.call_id
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| std::mem::take(&mut current_fn_call_id));
                        let name = event.name
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| std::mem::take(&mut current_fn_name));
                        let arguments = if !fn_args_accum.is_empty() {
                            std::mem::take(&mut fn_args_accum)
                        } else {
                            event.arguments.unwrap_or_default()
                        };
                        tool_calls.push(ToolCallInfo {
                            id: call_id,
                            name,
                            arguments,
                        });
                    }
                    "response.completed" => {
                        if let Some(ref resp_val) = event.response {
                            if let Some(u) = resp_val.get("usage") {
                                if let (Some(inp), Some(out)) = (
                                    u.get("input_tokens").and_then(|v| v.as_u64()),
                                    u.get("output_tokens").and_then(|v| v.as_u64()),
                                ) {
                                    usage = Some(TokenUsage {
                                        input_tokens: inp as u32,
                                        output_tokens: out as u32,
                                    });
                                }
                            }
                        }
                    }
                    _ => {
                        log::debug!("Unhandled SSE event type: {}", event.type_);
                    }
                }
            }

            if done {
                break;
            }
        }

        if !content.is_empty() {
            if let Some(tx) = tx {
                let _ = tx.send(StreamEvent::ContentDone(content.clone()));
            }
        }

        let content_opt = if content.is_empty() { None } else { Some(content) };

        Ok(LlmResponse {
            content: content_opt,
            tool_calls,
            usage,
        })
    }
}

#[async_trait]
impl LlmProvider for ChatgptProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<LlmResponse, LlmError> {
        log::info!(
            "ChatGPT chat request: model={}, messages={}, tools={}",
            self.model,
            messages.len(),
            tools.len()
        );
        let start = std::time::Instant::now();

        let result = self.send_streaming_request(messages, tools, None).await?;

        let elapsed = start.elapsed();
        let content_len = result.content.as_ref().map_or(0, |c| c.len());
        let tool_names: Vec<&str> = result.tool_calls.iter().map(|tc| tc.name.as_str()).collect();
        log::info!(
            "ChatGPT chat response: {:?}, content_len={}, tool_calls={:?}, tokens={:?}",
            elapsed,
            content_len,
            tool_names,
            result.usage.as_ref().map(|u| (u.input_tokens, u.output_tokens))
        );

        Ok(result)
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        tx: mpsc::UnboundedSender<StreamEvent>,
    ) -> Result<LlmResponse, LlmError> {
        log::info!(
            "ChatGPT chat_stream request: model={}, messages={}, tools={}",
            self.model,
            messages.len(),
            tools.len()
        );
        let start = std::time::Instant::now();

        let result = self.send_streaming_request(messages, tools, Some(&tx)).await?;

        let elapsed = start.elapsed();
        let content_len = result.content.as_ref().map_or(0, |c| c.len());
        let tool_names: Vec<&str> = result.tool_calls.iter().map(|tc| tc.name.as_str()).collect();
        log::info!(
            "ChatGPT chat_stream response: {:?}, content_len={}, tool_calls={:?}",
            elapsed,
            content_len,
            tool_names
        );

        Ok(result)
    }
}
