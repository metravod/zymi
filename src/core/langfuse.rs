use std::sync::{Arc, Mutex};

use base64::Engine;
use chrono::Utc;
use serde::Serialize;
use serde_json::{json, Value};

use super::Message;

pub struct LangfuseConfig {
    pub public_key: String,
    pub secret_key: String,
    pub host: String,
}

impl LangfuseConfig {
    pub fn from_env() -> Option<Self> {
        let public_key = std::env::var("LANGFUSE_PUBLIC_KEY").ok()?;
        let secret_key = std::env::var("LANGFUSE_SECRET_KEY").ok()?;
        let host = std::env::var("LANGFUSE_HOST")
            .unwrap_or_else(|_| "https://cloud.langfuse.com".to_string());
        Some(Self {
            public_key,
            secret_key,
            host,
        })
    }
}

#[derive(Serialize)]
struct IngestionBatch {
    batch: Vec<IngestionEvent>,
}

#[derive(Serialize)]
struct IngestionEvent {
    id: String,
    #[serde(rename = "type")]
    event_type: String,
    timestamp: String,
    body: Value,
}

pub struct LangfuseClient {
    auth_header: String,
    ingestion_url: String,
    http: reqwest::Client,
    buffer: Mutex<Vec<IngestionEvent>>,
}

impl LangfuseClient {
    pub fn new(config: LangfuseConfig) -> Arc<Self> {
        let auth = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", config.public_key, config.secret_key));
        let ingestion_url = format!(
            "{}/api/public/ingestion",
            config.host.trim_end_matches('/')
        );

        log::info!("LangFuse enabled: {}", config.host);

        Arc::new(Self {
            auth_header: format!("Basic {auth}"),
            ingestion_url,
            http: reqwest::Client::new(),
            buffer: Mutex::new(Vec::new()),
        })
    }

    fn push(&self, event_type: &str, body: Value) {
        let event = IngestionEvent {
            id: uuid::Uuid::new_v4().to_string(),
            event_type: event_type.to_string(),
            timestamp: now(),
            body,
        };
        if let Ok(mut buf) = self.buffer.lock() {
            buf.push(event);
        }
    }

    pub fn trace(&self, id: &str, name: &str, session_id: &str, input: Option<Value>, output: Option<Value>) {
        let mut body = json!({ "id": id, "name": name, "sessionId": session_id });
        if let Some(inp) = input {
            body["input"] = inp;
        }
        if let Some(out) = output {
            body["output"] = out;
        }
        self.push("trace-create", body);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn generation(
        &self,
        trace_id: &str,
        parent_id: Option<&str>,
        name: &str,
        model: &str,
        input: Value,
        output: Value,
        usage_input: u32,
        usage_output: u32,
        start_time: &str,
        end_time: &str,
    ) {
        let mut body = json!({
            "id": uuid::Uuid::new_v4().to_string(),
            "traceId": trace_id,
            "name": name,
            "model": model,
            "input": input,
            "output": output,
            "startTime": start_time,
            "endTime": end_time,
            "usage": {
                "input": usage_input,
                "output": usage_output,
                "unit": "TOKENS",
            },
        });
        if let Some(pid) = parent_id {
            body["parentObservationId"] = Value::String(pid.to_string());
        }
        self.push("generation-create", body);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn span(
        &self,
        trace_id: &str,
        parent_id: Option<&str>,
        name: &str,
        input: Value,
        output: Value,
        start_time: &str,
        end_time: &str,
        level: Option<&str>,
    ) {
        let mut body = json!({
            "id": uuid::Uuid::new_v4().to_string(),
            "traceId": trace_id,
            "name": name,
            "input": input,
            "output": output,
            "startTime": start_time,
            "endTime": end_time,
        });
        if let Some(pid) = parent_id {
            body["parentObservationId"] = Value::String(pid.to_string());
        }
        if let Some(lvl) = level {
            body["level"] = Value::String(lvl.to_string());
        }
        self.push("span-create", body);
    }

    pub async fn flush(&self) {
        let events = {
            let Ok(mut buf) = self.buffer.lock() else {
                return;
            };
            if buf.is_empty() {
                return;
            }
            std::mem::take(&mut *buf)
        };

        let count = events.len();
        let batch = IngestionBatch { batch: events };

        match self
            .http
            .post(&self.ingestion_url)
            .header("Authorization", &self.auth_header)
            .header("Content-Type", "application/json")
            .json(&batch)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                log::debug!("Langfuse: flushed {count} events");
            }
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                let preview = &body[..body.len().min(200)];
                log::warn!("Langfuse: flush failed ({status}): {preview}");
            }
            Err(e) => {
                log::warn!("Langfuse: flush error: {e}");
            }
        }
    }

    pub fn start_flushing(
        self: &Arc<Self>,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        let client = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => {
                        client.flush().await;
                        break;
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(3)) => {
                        client.flush().await;
                    }
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// TraceCtx — per-request tracing helper for the agent loop
// ---------------------------------------------------------------------------

pub struct TraceCtx {
    client: Arc<LangfuseClient>,
    trace_id: String,
    model_name: String,
}

impl TraceCtx {
    pub fn new(
        client: Arc<LangfuseClient>,
        model_name: &str,
        session_id: &str,
        user_message: &str,
    ) -> Self {
        let trace_id = uuid::Uuid::new_v4().to_string();
        client.trace(
            &trace_id,
            "agent.process",
            session_id,
            Some(json!({ "message": user_message })),
            None,
        );
        Self {
            client,
            trace_id,
            model_name: model_name.to_string(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_generation(
        &self,
        messages: &[Message],
        response_content: Option<&str>,
        tool_call_names: &[&str],
        usage_input: u32,
        usage_output: u32,
        start_time: &str,
        end_time: &str,
    ) {
        self.client.generation(
            &self.trace_id,
            None,
            "llm.chat",
            &self.model_name,
            messages_for_trace(messages),
            json!({
                "content": response_content,
                "tool_calls": tool_call_names,
            }),
            usage_input,
            usage_output,
            start_time,
            end_time,
        );
    }

    pub fn record_tool(
        &self,
        tool_name: &str,
        arguments: &str,
        result: &str,
        is_error: bool,
        start_time: &str,
        end_time: &str,
    ) {
        self.client.span(
            &self.trace_id,
            None,
            &format!("tool.{tool_name}"),
            json!({ "arguments": arguments }),
            json!({ "result": truncate(result, 1000), "is_error": is_error }),
            start_time,
            end_time,
            if is_error { Some("WARNING") } else { None },
        );
    }

    pub fn finish(&self, output: &str) {
        self.client.trace(
            &self.trace_id,
            "agent.process",
            "",
            None,
            Some(json!({ "message": output })),
        );
        let client = self.client.clone();
        tokio::spawn(async move {
            client.flush().await;
        });
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now() -> String {
    Utc::now()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub fn timestamp() -> String {
    now()
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        let end = s.floor_char_boundary(max);
        &s[..end]
    }
}

fn messages_for_trace(messages: &[Message]) -> Value {
    let compact: Vec<Value> = messages
        .iter()
        .map(|m| match m {
            Message::System(s) => json!({
                "role": "system",
                "content_len": s.len(),
            }),
            Message::User(s) => json!({
                "role": "user",
                "content": truncate(s, 500),
            }),
            Message::UserMultimodal { parts } => {
                let text = parts.iter().filter_map(|p| match p {
                    super::ContentPart::Text(t) => Some(t.as_str()),
                    _ => None,
                }).collect::<Vec<_>>().join(" ");
                let has_image = parts.iter().any(|p| matches!(p, super::ContentPart::ImageBase64 { .. }));
                json!({
                    "role": "user",
                    "content": truncate(&text, 500),
                    "has_image": has_image,
                })
            }
            Message::Assistant {
                content,
                tool_calls,
            } => json!({
                "role": "assistant",
                "content": content.as_deref().map(|c| truncate(c, 500)),
                "tool_calls": tool_calls.iter().map(|tc| &tc.name).collect::<Vec<_>>(),
            }),
            Message::ToolResult {
                tool_call_id,
                content,
            } => json!({
                "role": "tool",
                "tool_call_id": tool_call_id,
                "content_len": content.len(),
            }),
        })
        .collect();
    Value::Array(compact)
}
