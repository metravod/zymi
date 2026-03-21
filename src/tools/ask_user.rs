use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

use crate::core::ToolDefinition;

use super::Tool;

pub struct UserQuestion {
    pub question: String,
    pub responder: oneshot::Sender<String>,
}

pub struct AskUserTool {
    tx: mpsc::UnboundedSender<UserQuestion>,
}

impl AskUserTool {
    pub fn new(tx: mpsc::UnboundedSender<UserQuestion>) -> Self {
        Self { tx }
    }
}

#[async_trait]
impl Tool for AskUserTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "ask_user".to_string(),
            description: "Ask the user a question and wait for their response. \
                Use when you need clarification, a choice between options, \
                or additional input to proceed. Do not use for status updates \
                or confirmations — just for genuine questions."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "The question to ask the user. Be specific and concise."
                    }
                },
                "required": ["question"]
            }),
        }
    }

    async fn execute(&self, arguments: &str) -> Result<String, String> {
        let args: serde_json::Value =
            serde_json::from_str(arguments).map_err(|e| format!("Invalid arguments: {e}"))?;

        let question = args["question"]
            .as_str()
            .ok_or("Missing required parameter: question")?;

        let (responder, rx) = oneshot::channel();

        self.tx
            .send(UserQuestion {
                question: question.to_string(),
                responder,
            })
            .map_err(|_| "User input not available (non-interactive mode)".to_string())?;

        rx.await
            .map_err(|_| "User input cancelled".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn execute_returns_user_response() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let tool = AskUserTool::new(tx);

        let handle = tokio::spawn(async move {
            tool.execute(r#"{"question": "Which dir?"}"#).await
        });

        let q = rx.recv().await.unwrap();
        assert_eq!(q.question, "Which dir?");
        q.responder.send("/var/data".to_string()).unwrap();

        let result = handle.await.unwrap().unwrap();
        assert_eq!(result, "/var/data");
    }

    #[tokio::test]
    async fn execute_missing_question() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let tool = AskUserTool::new(tx);
        let result = tool.execute(r#"{}"#).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn execute_channel_closed() {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        let tool = AskUserTool::new(tx);
        let result = tool.execute(r#"{"question": "hello"}"#).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("non-interactive"));
    }
}
