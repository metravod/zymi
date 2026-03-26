use std::sync::Arc;

use uuid::Uuid;

use super::bus::EventBus;
use super::{Event, EventKind};

/// Adapter for connectors (Telegram, CLI, scheduler) to publish events and await responses.
///
/// Usage: the connector sets up its approval handler (ApprovalSlotGuard) as usual,
/// then calls `submit_and_wait()` instead of `agent.process_multimodal()`.
/// The approval handler remains active in the connector's scope while the
/// AgentWorker processes the event asynchronously.
pub struct EventDrivenConnector {
    bus: Arc<EventBus>,
}

impl EventDrivenConnector {
    pub fn new(bus: Arc<EventBus>) -> Self {
        Self { bus }
    }

    /// Publish a UserMessageReceived event and wait for the matching ResponseReady.
    ///
    /// Returns the response content, or an error if the timeout elapses.
    /// The caller should hold an ApprovalSlotGuard while this is in progress.
    pub async fn submit_and_wait(
        &self,
        conversation_id: &str,
        message: crate::core::Message,
        source: &str,
        timeout: std::time::Duration,
    ) -> Result<String, ConnectorError> {
        let correlation_id = Uuid::new_v4();

        // Subscribe BEFORE publishing to avoid race condition
        let mut rx = self.bus.subscribe().await;

        let event = Event::new(
            conversation_id.into(),
            EventKind::UserMessageReceived {
                content: message,
                connector: source.into(),
            },
            source.into(),
        )
        .with_correlation(correlation_id);

        self.bus
            .publish(event)
            .await
            .map_err(|e| ConnectorError::PublishFailed(e.to_string()))?;

        // Wait for ResponseReady with matching correlation_id
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);

        loop {
            tokio::select! {
                maybe_event = rx.recv() => {
                    match maybe_event {
                        Some(event) if event.correlation_id == Some(correlation_id) => {
                            if let EventKind::ResponseReady { content, .. } = event.kind {
                                return Ok(content);
                            }
                            // Other events with our correlation_id — keep waiting
                        }
                        Some(_) => continue, // Not our event
                        None => return Err(ConnectorError::BusClosed),
                    }
                }
                _ = &mut deadline => {
                    return Err(ConnectorError::Timeout);
                }
            }
        }
    }

    /// Fire-and-forget: publish an event without waiting for a response.
    /// Useful for scheduled tasks where the result goes to a notification channel.
    pub async fn submit(
        &self,
        conversation_id: &str,
        message: crate::core::Message,
        source: &str,
    ) -> Result<Uuid, ConnectorError> {
        let correlation_id = Uuid::new_v4();

        let event = Event::new(
            conversation_id.into(),
            EventKind::UserMessageReceived {
                content: message,
                connector: source.into(),
            },
            source.into(),
        )
        .with_correlation(correlation_id);

        self.bus
            .publish(event)
            .await
            .map_err(|e| ConnectorError::PublishFailed(e.to_string()))?;

        Ok(correlation_id)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectorError {
    #[error("failed to publish event: {0}")]
    PublishFailed(String),
    #[error("response timeout")]
    Timeout,
    #[error("event bus closed")]
    BusClosed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Message;
    use crate::events::store::SqliteEventStore;
    use tempfile::TempDir;

    async fn setup() -> (TempDir, Arc<EventBus>) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test_connector.db");
        let store = Arc::new(SqliteEventStore::new(&db_path).unwrap());
        let bus = Arc::new(EventBus::new(store));
        (dir, bus)
    }

    #[tokio::test]
    async fn submit_and_wait_receives_response() {
        let (_dir, bus) = setup().await;
        let connector = EventDrivenConnector::new(bus.clone());

        // Simulate an AgentWorker: subscribe and echo back ResponseReady
        let bus_clone = bus.clone();
        tokio::spawn(async move {
            let mut rx = bus_clone.subscribe().await;
            while let Some(event) = rx.recv().await {
                if let EventKind::UserMessageReceived { .. } = &event.kind {
                    let response = Event::new(
                        event.stream_id.clone(),
                        EventKind::ResponseReady {
                            conversation_id: event.stream_id,
                            content: "echo response".into(),
                        },
                        "mock_worker".into(),
                    )
                    .with_correlation(event.correlation_id.unwrap());

                    bus_clone.publish(response).await.unwrap();
                }
            }
        });

        let result = connector
            .submit_and_wait(
                "test-conv",
                Message::User("hello".into()),
                "test",
                std::time::Duration::from_secs(5),
            )
            .await
            .unwrap();

        assert_eq!(result, "echo response");
    }

    #[tokio::test]
    async fn submit_and_wait_timeout() {
        let (_dir, bus) = setup().await;
        let connector = EventDrivenConnector::new(bus);

        // No worker running — should timeout
        let result = connector
            .submit_and_wait(
                "test-conv",
                Message::User("hello".into()),
                "test",
                std::time::Duration::from_millis(50),
            )
            .await;

        assert!(matches!(result, Err(ConnectorError::Timeout)));
    }

    #[tokio::test]
    async fn submit_fire_and_forget() {
        let (_dir, bus) = setup().await;
        let connector = EventDrivenConnector::new(bus.clone());

        let corr = connector
            .submit("test-conv", Message::User("hello".into()), "scheduler")
            .await
            .unwrap();

        // Event should be in the store
        let events = bus.store().read_stream("test-conv", 1).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].correlation_id, Some(corr));
    }
}
