use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

use crate::core::approval::ApprovalHandler;
use super::app::PendingApproval;

pub enum AppEvent {
    ApprovalRequest(PendingApproval),
}

pub struct CliApprovalHandler {
    event_tx: mpsc::UnboundedSender<AppEvent>,
}

impl CliApprovalHandler {
    pub fn new(event_tx: mpsc::UnboundedSender<AppEvent>) -> Self {
        Self { event_tx }
    }
}

#[async_trait]
impl ApprovalHandler for CliApprovalHandler {
    async fn request_approval(
        &self,
        tool_description: &str,
        explanation: Option<&str>,
    ) -> Result<bool, String> {
        let (tx, rx) = oneshot::channel();

        let event = AppEvent::ApprovalRequest(PendingApproval {
            tool_description: tool_description.to_string(),
            explanation: explanation.map(|s| s.to_string()),
            responder: tx,
        });

        self.event_tx
            .send(event)
            .map_err(|_| "Failed to send approval request".to_string())?;

        rx.await.map_err(|_| "Approval channel closed".to_string())
    }
}
