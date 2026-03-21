use async_trait::async_trait;
use serde::Deserialize;

use crate::core::ToolDefinition;
use crate::tools::Tool;

pub struct WebSearchTool {
    api_key: String,
    client: reqwest::Client,
}

impl WebSearchTool {
    pub fn new() -> Option<Self> {
        let api_key = std::env::var("TAVILY_API_KEY").ok()?;
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
struct TavilyResponse {
    answer: Option<String>,
    results: Vec<TavilyResult>,
}

#[derive(Deserialize)]
struct TavilyResult {
    title: String,
    url: String,
    content: String,
}

#[async_trait]
impl Tool for WebSearchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "web_search".to_string(),
            description: "Search the internet using a query. Returns a summary and a list of relevant results with titles, URLs, and snippets.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query"
                    }
                },
                "required": ["query"]
            }),
        }
    }

    async fn execute(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid arguments: {e}"))?;

        let query = args["query"]
            .as_str()
            .ok_or("Missing required parameter: query")?;

        let body = serde_json::json!({
            "api_key": self.api_key,
            "query": query,
            "max_results": 5,
            "include_answer": true,
        });

        let resp = self
            .client
            .post("https://api.tavily.com/search")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("Tavily request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("Tavily API error {status}: {text}"));
        }

        let data: TavilyResponse = resp
            .json()
            .await
            .map_err(|e| format!("Failed to parse Tavily response: {e}"))?;

        let mut output = String::new();

        if let Some(answer) = &data.answer {
            output.push_str("**Summary:**\n");
            output.push_str(answer);
            output.push_str("\n\n");
        }

        output.push_str("**Results:**\n");
        for (i, result) in data.results.iter().enumerate() {
            output.push_str(&format!(
                "{}. {}\n   {}\n   {}\n\n",
                i + 1,
                result.title,
                result.url,
                result.content,
            ));
        }

        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_returns_none_without_env() {
        // Clear env var to ensure None
        std::env::remove_var("TAVILY_API_KEY");
        assert!(WebSearchTool::new().is_none());
    }

    #[test]
    fn new_returns_none_for_empty_key() {
        std::env::set_var("TAVILY_API_KEY", "");
        assert!(WebSearchTool::new().is_none());
        std::env::remove_var("TAVILY_API_KEY");
    }

    #[test]
    fn definition_has_correct_name() {
        std::env::set_var("TAVILY_API_KEY", "test-key");
        let tool = WebSearchTool::new().unwrap();
        assert_eq!(tool.definition().name, "web_search");
        std::env::remove_var("TAVILY_API_KEY");
    }

    #[tokio::test]
    async fn execute_invalid_json() {
        std::env::set_var("TAVILY_API_KEY", "test-key");
        let tool = WebSearchTool::new().unwrap();
        let result = tool.execute("not json").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid arguments"));
        std::env::remove_var("TAVILY_API_KEY");
    }

    #[tokio::test]
    async fn execute_missing_query() {
        std::env::set_var("TAVILY_API_KEY", "test-key");
        let tool = WebSearchTool::new().unwrap();
        let result = tool.execute("{}").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Missing"));
        std::env::remove_var("TAVILY_API_KEY");
    }

    #[test]
    fn tavily_response_parsing() {
        let json = r#"{
            "answer": "Test answer",
            "results": [
                {"title": "Result 1", "url": "https://example.com", "content": "Snippet 1"},
                {"title": "Result 2", "url": "https://example.org", "content": "Snippet 2"}
            ]
        }"#;
        let resp: TavilyResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.answer.as_deref(), Some("Test answer"));
        assert_eq!(resp.results.len(), 2);
        assert_eq!(resp.results[0].title, "Result 1");
    }

    #[test]
    fn tavily_response_no_answer() {
        let json = r#"{"results": []}"#;
        let resp: TavilyResponse = serde_json::from_str(json).unwrap();
        assert!(resp.answer.is_none());
        assert!(resp.results.is_empty());
    }
}
