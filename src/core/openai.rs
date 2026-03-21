use std::collections::HashMap;

use async_openai::{
    config::OpenAIConfig,
    types::{
        ChatCompletionMessageToolCall, ChatCompletionRequestAssistantMessageArgs,
        ChatCompletionRequestMessage, ChatCompletionRequestSystemMessageArgs,
        ChatCompletionRequestToolMessageArgs, ChatCompletionRequestUserMessageArgs,
        ChatCompletionToolArgs, ChatCompletionToolType, CreateChatCompletionRequestArgs,
        FunctionCall, FunctionObjectArgs,
    },
    Client,
};
use async_trait::async_trait;
use futures::StreamExt;
use tokio::sync::mpsc;

use super::{LlmError, LlmProvider, LlmResponse, Message, StreamEvent, TokenUsage, ToolCallInfo, ToolDefinition};

pub struct OpenAiProvider {
    client: Client<OpenAIConfig>,
    model: String,
}

impl OpenAiProvider {
    pub fn new(model: &str) -> Self {
        Self {
            client: Client::new(),
            model: model.to_string(),
        }
    }

    pub fn with_config(model: &str, api_key: &str, base_url: Option<&str>) -> Self {
        let mut config = OpenAIConfig::default().with_api_key(api_key);
        if let Some(url) = base_url {
            config = config.with_api_base(url);
        }
        Self {
            client: Client::with_config(config),
            model: model.to_string(),
        }
    }

}

fn convert_message(msg: &Message) -> Result<ChatCompletionRequestMessage, LlmError> {
    let map_err = |e: async_openai::error::OpenAIError| LlmError::RequestBuildError(e.to_string());

    match msg {
        Message::System(text) => Ok(ChatCompletionRequestSystemMessageArgs::default()
            .content(text.as_str())
            .build()
            .map_err(map_err)?
            .into()),
        Message::User(text) => Ok(ChatCompletionRequestUserMessageArgs::default()
            .content(text.as_str())
            .build()
            .map_err(map_err)?
            .into()),
        Message::Assistant {
            content,
            tool_calls,
        } => {
            let mut builder = ChatCompletionRequestAssistantMessageArgs::default();
            if let Some(text) = content {
                builder.content(text.as_str());
            }
            if !tool_calls.is_empty() {
                let calls: Vec<ChatCompletionMessageToolCall> = tool_calls
                    .iter()
                    .map(|tc| ChatCompletionMessageToolCall {
                        id: tc.id.clone(),
                        r#type: ChatCompletionToolType::Function,
                        function: FunctionCall {
                            name: tc.name.clone(),
                            arguments: tc.arguments.clone(),
                        },
                    })
                    .collect();
                builder.tool_calls(calls);
            }
            Ok(builder.build().map_err(map_err)?.into())
        }
        Message::ToolResult {
            tool_call_id,
            content,
        } => Ok(ChatCompletionRequestToolMessageArgs::default()
            .tool_call_id(tool_call_id.as_str())
            .content(content.as_str())
            .build()
            .map_err(map_err)?
            .into()),
    }
}

fn convert_tool(tool_def: &ToolDefinition) -> Result<async_openai::types::ChatCompletionTool, LlmError> {
    let map_err = |e: async_openai::error::OpenAIError| LlmError::RequestBuildError(e.to_string());

    let function = FunctionObjectArgs::default()
        .name(&tool_def.name)
        .description(&tool_def.description)
        .parameters(tool_def.parameters.clone())
        .build()
        .map_err(map_err)?;

    ChatCompletionToolArgs::default()
        .function(function)
        .build()
        .map_err(map_err)
}

fn build_request(
    model: &str,
    messages: &[Message],
    tools: &[ToolDefinition],
) -> Result<async_openai::types::CreateChatCompletionRequest, LlmError> {
    let oai_messages: Vec<ChatCompletionRequestMessage> = messages
        .iter()
        .map(convert_message)
        .collect::<Result<_, _>>()?;

    let mut request_builder = CreateChatCompletionRequestArgs::default();
    request_builder.model(model).messages(oai_messages);

    if !tools.is_empty() {
        let oai_tools: Vec<_> = tools
            .iter()
            .map(convert_tool)
            .collect::<Result<_, _>>()?;
        request_builder.tools(oai_tools);
    }

    request_builder
        .build()
        .map_err(|e| LlmError::RequestBuildError(e.to_string()))
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<LlmResponse, LlmError> {
        log::info!(
            "OpenAI chat request: model={}, messages={}, tools={}",
            self.model,
            messages.len(),
            tools.len()
        );
        let start = std::time::Instant::now();

        let request = build_request(&self.model, messages, tools)?;

        let response = self
            .client
            .chat()
            .create(request)
            .await
            .map_err(|e| {
                log::error!("OpenAI API error: {e}");
                LlmError::ApiError(e.to_string())
            })?;

        let elapsed = start.elapsed();
        let choice = response.choices.first().ok_or(LlmError::EmptyResponse)?;

        let tool_calls: Vec<ToolCallInfo> = choice
            .message
            .tool_calls
            .as_ref()
            .map(|calls| {
                calls
                    .iter()
                    .map(|tc| ToolCallInfo {
                        id: tc.id.clone(),
                        name: tc.function.name.clone(),
                        arguments: tc.function.arguments.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let usage = response.usage.map(|u| TokenUsage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
        });

        let content_len = choice.message.content.as_ref().map_or(0, |c| c.len());
        let tool_names: Vec<&str> = tool_calls.iter().map(|tc| tc.name.as_str()).collect();
        log::info!(
            "OpenAI chat response: {:?}, content_len={}, tool_calls={:?}, tokens={:?}",
            elapsed,
            content_len,
            tool_names,
            usage.as_ref().map(|u| (u.input_tokens, u.output_tokens))
        );

        Ok(LlmResponse {
            content: choice.message.content.clone(),
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
        log::info!(
            "OpenAI chat_stream request: model={}, messages={}, tools={}",
            self.model,
            messages.len(),
            tools.len()
        );
        let start = std::time::Instant::now();

        let request = build_request(&self.model, messages, tools)?;

        let mut stream = self
            .client
            .chat()
            .create_stream(request)
            .await
            .map_err(|e| {
                log::error!("OpenAI stream API error: {e}");
                LlmError::ApiError(e.to_string())
            })?;

        let mut content = String::new();
        let mut usage: Option<TokenUsage> = None;

        // Track tool calls by index as they arrive incrementally
        struct ToolCallAccum {
            id: String,
            name: String,
            arguments: String,
        }
        let mut tool_call_map: HashMap<u32, ToolCallAccum> = HashMap::new();

        while let Some(result) = stream.next().await {
            let response = result.map_err(|e| LlmError::ApiError(e.to_string()))?;

            // Capture usage from final chunk (if stream_options include_usage was set)
            if let Some(ref u) = response.usage {
                usage = Some(TokenUsage {
                    input_tokens: u.prompt_tokens,
                    output_tokens: u.completion_tokens,
                });
            }

            for choice in &response.choices {
                let delta = &choice.delta;

                // Stream text content
                if let Some(ref text) = delta.content {
                    content.push_str(text);
                    let _ = tx.send(StreamEvent::Token(text.clone()));
                }

                // Accumulate tool call chunks
                if let Some(ref tool_calls) = delta.tool_calls {
                    for tc_chunk in tool_calls {
                        let idx = tc_chunk.index;
                        let entry = tool_call_map.entry(idx).or_insert_with(|| ToolCallAccum {
                            id: String::new(),
                            name: String::new(),
                            arguments: String::new(),
                        });

                        if let Some(ref id) = tc_chunk.id {
                            entry.id = id.clone();
                        }
                        if let Some(ref func) = tc_chunk.function {
                            if let Some(ref name) = func.name {
                                entry.name = name.clone();
                            }
                            if let Some(ref args) = func.arguments {
                                entry.arguments.push_str(args);
                            }
                        }
                    }
                }
            }
        }

        // Build final tool calls from accumulated data
        let mut tool_calls: Vec<ToolCallInfo> = Vec::new();
        let mut indices: Vec<u32> = tool_call_map.keys().copied().collect();
        indices.sort();
        for idx in indices {
            // Safe: idx came from keys() above
            let accum = tool_call_map.remove(&idx).unwrap();
            tool_calls.push(ToolCallInfo {
                id: accum.id,
                name: accum.name,
                arguments: accum.arguments,
            });
        }

        if !content.is_empty() {
            let _ = tx.send(StreamEvent::ContentDone(content.clone()));
        }

        let content_opt = if content.is_empty() {
            None
        } else {
            Some(content)
        };

        let elapsed = start.elapsed();
        let content_len = content_opt.as_ref().map_or(0, |c| c.len());
        let tool_names: Vec<&str> = tool_calls.iter().map(|tc| tc.name.as_str()).collect();
        log::info!(
            "OpenAI chat_stream response: {:?}, content_len={}, tool_calls={:?}",
            elapsed,
            content_len,
            tool_names
        );

        Ok(LlmResponse {
            content: content_opt,
            tool_calls,
            usage,
        })
    }
}
