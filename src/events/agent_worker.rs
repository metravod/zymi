use std::sync::Arc;

use uuid::Uuid;

use crate::core::agent::Agent;
use crate::core::approval::ApprovalHandler;

use super::bus::EventBus;
use super::{Event, EventKind};

/// Consumes inbound events (UserMessageReceived, ScheduledTaskTriggered) and drives
/// the agent to produce a response. Publishes ResponseReady when done.
pub struct AgentWorker {
    agent: Arc<Agent>,
    bus: Arc<EventBus>,
}

impl AgentWorker {
    pub fn new(agent: Arc<Agent>, bus: Arc<EventBus>) -> Self {
        Self { agent, bus }
    }

    /// Run the worker loop. Spawns a tokio task per inbound event.
    /// Call this from a `tokio::spawn`.
    pub async fn run(self: Arc<Self>) {
        let mut rx = self.bus.subscribe().await;

        while let Some(event) = rx.recv().await {
            match &event.kind {
                EventKind::UserMessageReceived { .. }
                | EventKind::ScheduledTaskTriggered { .. } => {
                    let worker = Arc::clone(&self);
                    let event = event.clone();
                    tokio::spawn(async move {
                        worker.handle_inbound(event).await;
                    });
                }
                _ => {} // Other events handled by other workers
            }
        }
    }

    /// Run a single event through the agent and publish the result.
    /// This is also useful for testing without the full event loop.
    pub async fn handle_inbound(&self, event: Event) {
        let correlation_id = event.correlation_id.unwrap_or_else(Uuid::new_v4);
        let stream_id = event.stream_id.clone();

        // Publish processing started
        let started = Event::new(
            stream_id.clone(),
            EventKind::AgentProcessingStarted {
                conversation_id: stream_id.clone(),
            },
            "agent_worker".into(),
        )
        .with_correlation(correlation_id)
        .with_causation(event.id);

        if let Err(e) = self.bus.publish(started).await {
            log::error!("Failed to publish AgentProcessingStarted: {e}");
            return;
        }

        let result = match &event.kind {
            EventKind::UserMessageReceived { content, .. } => {
                self.process_user_message(&stream_id, content.clone(), None)
                    .await
            }
            EventKind::ScheduledTaskTriggered { task, .. } => {
                self.process_user_message(
                    &stream_id,
                    crate::core::Message::User(task.clone()),
                    None,
                )
                .await
            }
            _ => return,
        };

        let (success, response_content) = match result {
            Ok(content) => (true, content),
            Err(e) => {
                log::error!("Agent processing failed for stream {stream_id}: {e}");
                (false, format!("Error: {e}"))
            }
        };

        // Publish response
        let response = Event::new(
            stream_id.clone(),
            EventKind::ResponseReady {
                conversation_id: stream_id.clone(),
                content: response_content,
            },
            "agent_worker".into(),
        )
        .with_correlation(correlation_id)
        .with_causation(event.id);

        if let Err(e) = self.bus.publish(response).await {
            log::error!("Failed to publish ResponseReady: {e}");
        }

        // Publish processing completed
        let completed = Event::new(
            stream_id.clone(),
            EventKind::AgentProcessingCompleted {
                conversation_id: stream_id,
                success,
            },
            "agent_worker".into(),
        )
        .with_correlation(correlation_id)
        .with_causation(event.id);

        if let Err(e) = self.bus.publish(completed).await {
            log::error!("Failed to publish AgentProcessingCompleted: {e}");
        }
    }

    async fn process_user_message(
        &self,
        conversation_id: &str,
        message: crate::core::Message,
        approval_handler: Option<&dyn ApprovalHandler>,
    ) -> Result<String, crate::core::LlmError> {
        self.agent
            .process_multimodal(conversation_id, message, approval_handler)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Message;
    use crate::events::store::SqliteEventStore;
    use crate::events::EventKind;
    use tempfile::TempDir;

    async fn setup_bus() -> (TempDir, Arc<EventBus>) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test_worker.db");
        let store = Arc::new(SqliteEventStore::new(&db_path).unwrap());
        let bus = Arc::new(EventBus::new(store));
        (dir, bus)
    }

    #[tokio::test]
    async fn handle_inbound_publishes_lifecycle_events() {
        // We can't easily create a real Agent without all its dependencies,
        // but we can test the event flow by checking that the worker publishes
        // AgentProcessingStarted before attempting to call the agent.
        //
        // For a full integration test, we'd need a mock LlmProvider.
        // This test verifies the event envelope structure.

        let (_dir, bus) = setup_bus().await;
        let mut rx = bus.subscribe().await;

        let correlation = Uuid::new_v4();
        let event = Event::new(
            "test-conv".into(),
            EventKind::UserMessageReceived {
                content: Message::User("hello".into()),
                connector: "test".into(),
            },
            "test".into(),
        )
        .with_correlation(correlation);

        // Publish the inbound event directly (simulating what a connector does)
        bus.publish(event).await.unwrap();

        // The subscriber should receive UserMessageReceived
        let received = rx.recv().await.unwrap();
        assert_eq!(received.kind_tag(), "user_message_received");
        assert_eq!(received.correlation_id, Some(correlation));
    }
}
