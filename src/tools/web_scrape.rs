use async_trait::async_trait;
use serde::Deserialize;

use crate::core::ToolDefinition;
use crate::tools::Tool;

const MAX_CONTENT_LENGTH: usize = 15000;

pub struct WebScrapeTool {
    api_key: String,
    client: reqwest::Client,
}

impl WebScrapeTool {
    pub fn new() -> Option<Self> {
        let api_key = std::env::var("FIRECRAWL_API_KEY").ok()?;
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
struct FirecrawlResponse {
    success: bool,
    data: Option<FirecrawlData>,
}

#[derive(Deserialize)]
struct FirecrawlData {
    markdown: Option<String>,
}

#[async_trait]
impl Tool for WebScrapeTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "web_scrape".to_string(),
            description: "Scrape a web page and return its content as markdown. Use this to read the full content of a specific URL.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "The URL of the web page to scrape"
                    }
                },
                "required": ["url"]
            }),
        }
    }

    fn requires_approval(&self) -> bool {
        true
    }

    fn to_intention(&self, arguments: &str) -> Option<crate::esaa::Intention> {
        let args: serde_json::Value = serde_json::from_str(arguments).ok()?;
        let url = args["url"].as_str()?.to_string();
        Some(crate::esaa::Intention::WebScrape { url })
    }

    fn format_approval_request(&self, arguments: &str) -> String {
        let url = serde_json::from_str::<serde_json::Value>(arguments)
            .ok()
            .and_then(|v| v["url"].as_str().map(String::from))
            .unwrap_or_else(|| arguments.to_string());
        format!("URL:\n<code>{}</code>", url)
    }

    async fn execute(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid arguments: {e}"))?;

        let url = args["url"]
            .as_str()
            .ok_or("Missing required parameter: url")?;

        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err("Only http:// and https:// URLs are allowed.".to_string());
        }

        let body = serde_json::json!({
            "url": url,
            "formats": ["markdown"],
        });

        let resp = self
            .client
            .post("https://api.firecrawl.dev/v1/scrape")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Firecrawl request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("Firecrawl API error {status}: {text}"));
        }

        let data: FirecrawlResponse = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse Firecrawl response: {e}"))?;

        if !data.success {
            return Err("Firecrawl reported failure".to_string());
        }

        let markdown = data
            .data
            .and_then(|d| d.markdown)
            .unwrap_or_else(|| "No content returned.".to_string());

        if markdown.len() > MAX_CONTENT_LENGTH {
            let truncated: String = markdown.chars().take(MAX_CONTENT_LENGTH).collect();
            Ok(format!("{truncated}\n\n[Content truncated at {MAX_CONTENT_LENGTH} characters]"))
        } else {
            Ok(markdown)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(url: &str) -> String {
        serde_json::json!({ "url": url }).to_string()
    }

    #[test]
    fn new_returns_none_without_env() {
        std::env::remove_var("FIRECRAWL_API_KEY");
        assert!(WebScrapeTool::new().is_none());
    }

    #[test]
    fn new_returns_none_for_empty_key() {
        std::env::set_var("FIRECRAWL_API_KEY", "");
        assert!(WebScrapeTool::new().is_none());
        std::env::remove_var("FIRECRAWL_API_KEY");
    }

    #[test]
    fn definition_has_correct_name() {
        std::env::set_var("FIRECRAWL_API_KEY", "test-key");
        let tool = WebScrapeTool::new().unwrap();
        assert_eq!(tool.definition().name, "web_scrape");
        std::env::remove_var("FIRECRAWL_API_KEY");
    }

    #[test]
    fn requires_approval_true() {
        std::env::set_var("FIRECRAWL_API_KEY", "test-key");
        let tool = WebScrapeTool::new().unwrap();
        assert!(tool.requires_approval());
        std::env::remove_var("FIRECRAWL_API_KEY");
    }

    #[tokio::test]
    async fn execute_invalid_json() {
        std::env::set_var("FIRECRAWL_API_KEY", "test-key");
        let tool = WebScrapeTool::new().unwrap();
        let result = tool.execute("not json").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid arguments"));
        std::env::remove_var("FIRECRAWL_API_KEY");
    }

    #[tokio::test]
    async fn execute_missing_url() {
        std::env::set_var("FIRECRAWL_API_KEY", "test-key");
        let tool = WebScrapeTool::new().unwrap();
        let result = tool.execute("{}").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Missing"));
        std::env::remove_var("FIRECRAWL_API_KEY");
    }

    #[tokio::test]
    async fn execute_rejects_non_http_url() {
        std::env::set_var("FIRECRAWL_API_KEY", "test-key");
        let tool = WebScrapeTool::new().unwrap();
        let result = tool.execute(&args("javascript:alert(1)")).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("http"));
        std::env::remove_var("FIRECRAWL_API_KEY");
    }

    #[tokio::test]
    async fn execute_rejects_data_uri() {
        std::env::set_var("FIRECRAWL_API_KEY", "test-key");
        let tool = WebScrapeTool::new().unwrap();
        let result = tool.execute(&args("data:text/html,<h1>hi</h1>")).await;
        assert!(result.is_err());
        std::env::remove_var("FIRECRAWL_API_KEY");
    }

    #[test]
    fn format_approval_shows_url() {
        std::env::set_var("FIRECRAWL_API_KEY", "test-key");
        let tool = WebScrapeTool::new().unwrap();
        let desc = tool.format_approval_request(&args("https://example.com"));
        assert!(desc.contains("https://example.com"));
        std::env::remove_var("FIRECRAWL_API_KEY");
    }

    #[test]
    fn firecrawl_response_parsing() {
        let json = r#"{"success": true, "data": {"markdown": "Hello World"}}"#;
        let resp: FirecrawlResponse = serde_json::from_str(json).unwrap();
        assert!(resp.success);
        assert_eq!(resp.data.unwrap().markdown.as_deref(), Some("Hello World"));
    }

    #[test]
    fn firecrawl_response_no_data() {
        let json = r#"{"success": false, "data": null}"#;
        let resp: FirecrawlResponse = serde_json::from_str(json).unwrap();
        assert!(!resp.success);
        assert!(resp.data.is_none());
    }
}
