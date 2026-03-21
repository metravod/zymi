use async_trait::async_trait;
use serde::Deserialize;

use crate::core::ToolDefinition;
use crate::tools::Tool;

const API_BASE: &str = "https://api.supadata.ai/v1/transcript";
const MAX_TRANSCRIPT_LENGTH: usize = 15000;
const POLL_INTERVAL_SECS: u64 = 3;
const MAX_POLL_ATTEMPTS: u32 = 60; // 3 minutes max

pub struct YouTubeTranscriptTool {
    api_key: String,
    client: reqwest::Client,
}

impl YouTubeTranscriptTool {
    pub fn new() -> Option<Self> {
        let api_key = std::env::var("SUPADATA_API_KEY").ok()?;
        if api_key.is_empty() {
            return None;
        }
        Some(Self {
            api_key,
            client: reqwest::Client::new(),
        })
    }
}

#[derive(Deserialize)]
struct TranscriptResponse {
    content: Option<serde_json::Value>,
    lang: Option<String>,
    #[serde(rename = "availableLangs")]
    available_langs: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct AsyncResponse {
    #[serde(rename = "jobId")]
    job_id: String,
}

#[async_trait]
impl Tool for YouTubeTranscriptTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "youtube_transcript".to_string(),
            description: "Get the transcript/subtitles of a YouTube video. Returns the full text content of the video. Use this to analyze, summarize, or answer questions about video content.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "YouTube video URL (e.g. https://www.youtube.com/watch?v=... or https://youtu.be/...)"
                    },
                    "lang": {
                        "type": "string",
                        "description": "Preferred transcript language (ISO 639-1 code, e.g. en, ru, es). Falls back to first available if not found."
                    }
                },
                "required": ["url"]
            }),
        }
    }

    fn requires_approval(&self) -> bool {
        true
    }

    fn format_approval_request(&self, arguments: &str) -> String {
        let url = serde_json::from_str::<serde_json::Value>(arguments)
            .ok()
            .and_then(|v| v["url"].as_str().map(String::from))
            .unwrap_or_else(|| arguments.to_string());
        format!("YouTube transcript:\n<code>{}</code>", url)
    }

    async fn execute(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid arguments: {e}"))?;

        let url = args["url"]
            .as_str()
            .ok_or("Missing required parameter: url")?;

        if !is_youtube_url(url) {
            return Err("URL must be a YouTube video link (youtube.com or youtu.be)".to_string());
        }

        let mut request = self
            .client
            .get(API_BASE)
            .header("x-api-key", &self.api_key)
            .query(&[("url", url), ("text", "true")]);

        if let Some(lang) = args["lang"].as_str() {
            request = request.query(&[("lang", lang)]);
        }

        let resp = request
            .send()
            .await
            .map_err(|e| format!("Supadata request failed: {e}"))?;

        let status = resp.status();

        if status == reqwest::StatusCode::ACCEPTED {
            // Async processing — poll for result
            let async_resp: AsyncResponse = resp
                .json()
                .await
                .map_err(|e| format!("Failed to parse async response: {e}"))?;

            log::info!("YouTube transcript job queued: {}", async_resp.job_id);
            return self.poll_job(&async_resp.job_id).await;
        }

        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("Supadata API error {status}: {text}"));
        }

        let data: TranscriptResponse = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse transcript response: {e}"))?;

        format_transcript(data)
    }
}

impl YouTubeTranscriptTool {
    async fn poll_job(&self, job_id: &str) -> Result<String, String> {
        let url = format!("{API_BASE}/{job_id}");

        for attempt in 0..MAX_POLL_ATTEMPTS {
            tokio::time::sleep(std::time::Duration::from_secs(POLL_INTERVAL_SECS)).await;

            let resp = self
                .client
                .get(&url)
                .header("x-api-key", &self.api_key)
                .send()
                .await
                .map_err(|e| format!("Failed to poll transcript job: {e}"))?;

            let status = resp.status();

            if status == reqwest::StatusCode::ACCEPTED {
                // Still processing
                if (attempt + 1) % 10 == 0 {
                    log::info!(
                        "Transcript job {} still processing ({}s elapsed)",
                        job_id,
                        (attempt + 1) as u64 * POLL_INTERVAL_SECS
                    );
                }
                continue;
            }

            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                return Err(format!("Transcript job failed ({status}): {text}"));
            }

            let data: TranscriptResponse = resp
                .json()
                .await
                .map_err(|e| format!("Failed to parse transcript response: {e}"))?;

            return format_transcript(data);
        }

        Err(format!(
            "Transcript job {job_id} timed out after {}s",
            MAX_POLL_ATTEMPTS as u64 * POLL_INTERVAL_SECS
        ))
    }
}

fn is_youtube_url(url: &str) -> bool {
    let lower = url.to_lowercase();
    lower.contains("youtube.com/") || lower.contains("youtu.be/")
}

fn format_transcript(data: TranscriptResponse) -> Result<String, String> {
    let lang = data.lang.unwrap_or_else(|| "unknown".to_string());
    let available = data
        .available_langs
        .map(|langs| langs.join(", "))
        .unwrap_or_default();

    let text = match data.content {
        Some(serde_json::Value::String(s)) => s,
        Some(other) => serde_json::to_string_pretty(&other).unwrap_or_default(),
        None => return Err("No transcript available for this video.".to_string()),
    };

    if text.is_empty() {
        return Err("No transcript available for this video.".to_string());
    }

    let display = if text.len() > MAX_TRANSCRIPT_LENGTH {
        let truncated: String = text.chars().take(MAX_TRANSCRIPT_LENGTH).collect();
        format!("{truncated}\n\n[Transcript truncated at {MAX_TRANSCRIPT_LENGTH} characters]")
    } else {
        text
    };

    let mut header = format!("**Language:** {lang}");
    if !available.is_empty() {
        header.push_str(&format!("\n**Available languages:** {available}"));
    }

    Ok(format!("{header}\n\n---\n\n{display}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(url: &str) -> String {
        serde_json::json!({ "url": url }).to_string()
    }

    #[test]
    fn new_returns_none_without_env() {
        std::env::remove_var("SUPADATA_API_KEY");
        assert!(YouTubeTranscriptTool::new().is_none());
    }

    #[test]
    fn new_returns_none_for_empty_key() {
        std::env::set_var("SUPADATA_API_KEY", "");
        assert!(YouTubeTranscriptTool::new().is_none());
        std::env::remove_var("SUPADATA_API_KEY");
    }

    #[test]
    fn definition_has_correct_name() {
        std::env::set_var("SUPADATA_API_KEY", "test-key");
        let tool = YouTubeTranscriptTool::new().unwrap();
        assert_eq!(tool.definition().name, "youtube_transcript");
        std::env::remove_var("SUPADATA_API_KEY");
    }

    #[test]
    fn requires_approval_true() {
        std::env::set_var("SUPADATA_API_KEY", "test-key");
        let tool = YouTubeTranscriptTool::new().unwrap();
        assert!(tool.requires_approval());
        std::env::remove_var("SUPADATA_API_KEY");
    }

    #[tokio::test]
    async fn execute_invalid_json() {
        std::env::set_var("SUPADATA_API_KEY", "test-key");
        let tool = YouTubeTranscriptTool::new().unwrap();
        let result = tool.execute("not json").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid arguments"));
        std::env::remove_var("SUPADATA_API_KEY");
    }

    #[tokio::test]
    async fn execute_missing_url() {
        std::env::set_var("SUPADATA_API_KEY", "test-key");
        let tool = YouTubeTranscriptTool::new().unwrap();
        let result = tool.execute("{}").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("url"));
        std::env::remove_var("SUPADATA_API_KEY");
    }

    #[tokio::test]
    async fn execute_rejects_non_youtube_url() {
        std::env::set_var("SUPADATA_API_KEY", "test-key");
        let tool = YouTubeTranscriptTool::new().unwrap();
        let result = tool.execute(&args("https://vimeo.com/12345")).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("YouTube"));
        std::env::remove_var("SUPADATA_API_KEY");
    }

    #[test]
    fn is_youtube_url_valid() {
        assert!(is_youtube_url("https://www.youtube.com/watch?v=abc123"));
        assert!(is_youtube_url("https://youtu.be/abc123"));
        assert!(is_youtube_url("https://YOUTUBE.COM/watch?v=abc123"));
        assert!(!is_youtube_url("https://vimeo.com/12345"));
        assert!(!is_youtube_url("https://example.com"));
    }

    #[test]
    fn format_approval_shows_url() {
        std::env::set_var("SUPADATA_API_KEY", "test-key");
        let tool = YouTubeTranscriptTool::new().unwrap();
        let desc = tool.format_approval_request(&args("https://youtu.be/abc123"));
        assert!(desc.contains("youtu.be/abc123"));
        std::env::remove_var("SUPADATA_API_KEY");
    }

    #[test]
    fn format_transcript_plain_text() {
        let data = TranscriptResponse {
            content: Some(serde_json::Value::String("Hello world".to_string())),
            lang: Some("en".to_string()),
            available_langs: Some(vec!["en".to_string(), "ru".to_string()]),
        };
        let result = format_transcript(data).unwrap();
        assert!(result.contains("Hello world"));
        assert!(result.contains("en"));
        assert!(result.contains("en, ru"));
    }

    #[test]
    fn format_transcript_empty_content() {
        let data = TranscriptResponse {
            content: None,
            lang: None,
            available_langs: None,
        };
        let result = format_transcript(data);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("No transcript"));
    }

    #[test]
    fn format_transcript_truncation() {
        let long_text = "a".repeat(MAX_TRANSCRIPT_LENGTH + 1000);
        let data = TranscriptResponse {
            content: Some(serde_json::Value::String(long_text)),
            lang: Some("en".to_string()),
            available_langs: None,
        };
        let result = format_transcript(data).unwrap();
        assert!(result.contains("[Transcript truncated"));
    }
}
