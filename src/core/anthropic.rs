use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use super::{ContentPart, LlmError, LlmProvider, LlmResponse, Message, StreamEvent, TokenUsage, ToolCallInfo, ToolDefinition};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const MAX_TOKENS: u32 = 8192;

pub struct AnthropicProvider {
    client: reqwest::Client,
    model: String,
    base_url: String,
}

// --- Request types ---

#[derive(Serialize)]
struct ApiRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<ApiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ApiTool>,
}

#[derive(Serialize)]
struct ApiMessage {
    role: String,
    content: ApiContent,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ApiContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Serialize)]
struct ImageSource {
    #[serde(rename = "type")]
    type_: String,
    media_type: String,
    data: String,
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image { source: ImageSource },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
    },
}

#[derive(Serialize)]
struct ApiTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

// --- Response types ---

#[derive(Deserialize)]
struct ApiUsage {
    input_tokens: u32,
    output_tokens: u32,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct ApiResponse {
    content: Vec<ResponseBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<ApiUsage>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ResponseBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

#[derive(Deserialize)]
struct ApiErrorResponse {
    error: ApiErrorDetail,
}

#[derive(Deserialize)]
struct ApiErrorDetail {
    message: String,
}

impl AnthropicProvider {
    pub fn new(model: &str, api_key: &str, base_url: Option<&str>) -> Result<Self, String> {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-api-key",
            HeaderValue::from_str(api_key)
                .map_err(|e| format!("Invalid API key (non-ASCII characters?): {e}"))?,
        );
        headers.insert(
            "anthropic-version",
            HeaderValue::from_static(ANTHROPIC_VERSION),
        );

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .map_err(|e| format!("Failed to build HTTP client: {e}"))?;

        Ok(Self {
            client,
            model: model.to_string(),
            base_url: base_url
                .unwrap_or(DEFAULT_BASE_URL)
                .trim_end_matches('/')
                .to_string(),
        })
    }
}

fn convert_messages(messages: &[Message]) -> (Option<String>, Vec<ApiMessage>) {
    let mut system_prompt = None;
    let mut api_messages: Vec<ApiMessage> = Vec::new();

    for msg in messages {
        match msg {
            Message::System(text) => {
                system_prompt = Some(text.clone());
            }
            Message::User(text) => {
                api_messages.push(ApiMessage {
                    role: "user".to_string(),
                    content: ApiContent::Text(text.clone()),
                });
            }
            Message::UserMultimodal { parts } => {
                let blocks: Vec<ContentBlock> = parts
                    .iter()
                    .map(|p| match p {
                        ContentPart::Text(text) => ContentBlock::Text { text: text.clone() },
                        ContentPart::ImageBase64 { media_type, data } => ContentBlock::Image {
                            source: ImageSource {
                                type_: "base64".to_string(),
                                media_type: media_type.clone(),
                                data: data.clone(),
                            },
                        },
                    })
                    .collect();
                api_messages.push(ApiMessage {
                    role: "user".to_string(),
                    content: ApiContent::Blocks(blocks),
                });
            }
            Message::Assistant {
                content,
                tool_calls,
            } => {
                let mut blocks = Vec::new();

                if let Some(text) = content {
                    if !text.is_empty() {
                        blocks.push(ContentBlock::Text { text: text.clone() });
                    }
                }

                for tc in tool_calls {
                    let input: serde_json::Value =
                        serde_json::from_str(&tc.arguments).unwrap_or_else(|e| {
                            log::warn!("Failed to parse tool call arguments as JSON: {e}");
                            serde_json::Value::Object(serde_json::Map::new())
                        });
                    blocks.push(ContentBlock::ToolUse {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        input,
                    });
                }

                if blocks.is_empty() {
                    blocks.push(ContentBlock::Text {
                        text: String::new(),
                    });
                }

                api_messages.push(ApiMessage {
                    role: "assistant".to_string(),
                    content: ApiContent::Blocks(blocks),
                });
            }
            Message::ToolResult {
                tool_call_id,
                content,
            } => {
                let block = ContentBlock::ToolResult {
                    tool_use_id: tool_call_id.clone(),
                    content: content.clone(),
                };

                // Anthropic requires alternating user/assistant turns.
                // Merge consecutive tool results into a single user message.
                if let Some(last) = api_messages.last_mut() {
                    if last.role == "user" {
                        if let ApiContent::Blocks(ref mut blocks) = last.content {
                            blocks.push(block);
                            continue;
                        }
                    }
                }

                api_messages.push(ApiMessage {
                    role: "user".to_string(),
                    content: ApiContent::Blocks(vec![block]),
                });
            }
        }
    }

    (system_prompt, api_messages)
}

fn convert_tools(tools: &[ToolDefinition]) -> Vec<ApiTool> {
    tools
        .iter()
        .map(|t| ApiTool {
            name: t.name.clone(),
            description: t.description.clone(),
            input_schema: t.parameters.clone(),
        })
        .collect()
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<LlmResponse, LlmError> {
        log::info!(
            "Anthropic chat request: model={}, messages={}, tools={}",
            self.model,
            messages.len(),
            tools.len()
        );
        let start = std::time::Instant::now();

        let (system, api_messages) = convert_messages(messages);
        let api_tools = convert_tools(tools);

        let request = ApiRequest {
            model: self.model.clone(),
            max_tokens: MAX_TOKENS,
            messages: api_messages,
            system,
            tools: api_tools,
        };

        let url = format!("{}/v1/messages", self.base_url);

        let response = self
            .client
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| {
                log::error!("Anthropic API request error: {e}");
                LlmError::ApiError(e.to_string())
            })?;

        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| LlmError::ApiError(e.to_string()))?;

        if !status.is_success() {
            let error_msg = match serde_json::from_str::<ApiErrorResponse>(&body) {
                Ok(err) => err.error.message,
                Err(_) => body,
            };
            log::error!("Anthropic API error ({}): {}", status, error_msg);
            return Err(LlmError::ApiError(format!(
                "Anthropic API error ({}): {}",
                status, error_msg
            )));
        }

        let api_response: ApiResponse = serde_json::from_str(&body)
            .map_err(|e| LlmError::ApiError(format!("Failed to parse response: {e}")))?;

        let mut content_parts: Vec<String> = Vec::new();
        let mut tool_calls: Vec<ToolCallInfo> = Vec::new();

        for block in api_response.content {
            match block {
                ResponseBlock::Text { text } => {
                    if !text.is_empty() {
                        content_parts.push(text);
                    }
                }
                ResponseBlock::ToolUse { id, name, input } => {
                    tool_calls.push(ToolCallInfo {
                        id,
                        name,
                        arguments: serde_json::to_string(&input).unwrap_or_else(|e| {
                            log::warn!("Failed to serialize tool input: {e}");
                            String::new()
                        }),
                    });
                }
            }
        }

        let content = if content_parts.is_empty() {
            None
        } else {
            Some(content_parts.join("\n"))
        };

        let usage = api_response.usage.map(|u| TokenUsage {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
        });

        let elapsed = start.elapsed();
        let content_len = content.as_ref().map_or(0, |c| c.len());
        let tool_names: Vec<&str> = tool_calls.iter().map(|tc| tc.name.as_str()).collect();
        log::info!(
            "Anthropic chat response: {:?}, content_len={}, tool_calls={:?}, tokens={:?}",
            elapsed,
            content_len,
            tool_names,
            usage.as_ref().map(|u| (u.input_tokens, u.output_tokens))
        );

        Ok(LlmResponse {
            content,
            tool_calls,
            usage,
        })
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
        tx: mpsc::UnboundedSender<StreamEvent>,
    ) -> Result<LlmResponse, LlmError> {
        log::info!("Anthropic chat_stream: falling back to non-streaming chat");
        let response = self.chat(messages, tools).await?;
        if let Some(ref content) = response.content {
            let _ = tx.send(StreamEvent::Token(content.clone()));
            let _ = tx.send(StreamEvent::ContentDone(content.clone()));
        }
        Ok(response)
    }
}
