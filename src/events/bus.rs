use std::sync::Arc;

use tokio::sync::{mpsc, RwLock};

use super::store::EventStore;
use super::{Event, EventStoreError};

/// In-process event bus. Persists events to the store, then fans out to subscribers.
pub struct EventBus {
    store: Arc<dyn EventStore>,
    subscribers: RwLock<Vec<mpsc::UnboundedSender<Event>>>,
}

impl EventBus {
    pub fn new(store: Arc<dyn EventStore>) -> Self {
        Self {
            store,
            subscribers: RwLock::new(Vec::new()),
        }
    }

    /// Persist the event to the store (source of truth), then deliver to all subscribers.
    /// The event's sequence number is assigned by the store.
    pub async fn publish(&self, mut event: Event) -> Result<(), EventStoreError> {
        self.store.append(&mut event).await?;

        let subs = self.subscribers.read().await;
        // Remove closed channels lazily on next subscribe/publish
        for tx in subs.iter() {
            // Ignore send errors -- subscriber may have dropped
            let _ = tx.send(event.clone());
        }
        Ok(())
    }

    /// Subscribe to all events. Returns a receiver that gets a copy of every published event.
    pub async fn subscribe(&self) -> mpsc::UnboundedReceiver<Event> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut subs = self.subscribers.write().await;
        // Clean up dead subscribers while we have the write lock
        subs.retain(|tx| !tx.is_closed());
        subs.push(tx);
        rx
    }

    /// Access the underlying store for replay/queries.
    pub fn store(&self) -> &Arc<dyn EventStore> {
        &self.store
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Message;
    use crate::events::store::SqliteEventStore;
    use crate::events::EventKind;
    use tempfile::TempDir;

    async fn setup() -> (TempDir, Arc<EventBus>) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test_bus.db");
        let store = Arc::new(SqliteEventStore::new(&db_path).unwrap());
        let bus = Arc::new(EventBus::new(store));
        (dir, bus)
    }

    #[tokio::test]
    async fn publish_persists_and_delivers() {
        let (_dir, bus) = setup().await;
        let mut rx = bus.subscribe().await;

        let event = Event::new(
            "s1".into(),
            EventKind::UserMessageReceived {
                content: Message::User("hello".into()),
                connector: "test".into(),
            },
            "test".into(),
        );

        bus.publish(event).await.unwrap();

        // Subscriber receives the event
        let received = rx.try_recv().unwrap();
        assert_eq!(received.kind_tag(), "user_message_received");
        assert_eq!(received.sequence, 1); // assigned by store

        // Event is persisted
        let stored = bus.store().read_stream("s1", 1).await.unwrap();
        assert_eq!(stored.len(), 1);
    }

    #[tokio::test]
    async fn multiple_subscribers_all_receive() {
        let (_dir, bus) = setup().await;
        let mut rx1 = bus.subscribe().await;
        let mut rx2 = bus.subscribe().await;
        let mut rx3 = bus.subscribe().await;

        let event = Event::new(
            "s1".into(),
            EventKind::AgentProcessingStarted {
                conversation_id: "s1".into(),
            },
            "agent".into(),
        );

        bus.publish(event).await.unwrap();

        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_ok());
        assert!(rx3.try_recv().is_ok());
    }

    #[tokio::test]
    async fn dropped_subscriber_does_not_block() {
        let (_dir, bus) = setup().await;
        let rx1 = bus.subscribe().await;
        let mut rx2 = bus.subscribe().await;

        // Drop first subscriber
        drop(rx1);

        let event = Event::new(
            "s1".into(),
            EventKind::ResponseReady {
                conversation_id: "s1".into(),
                content: "done".into(),
            },
            "agent".into(),
        );

        // Should not error even though rx1 is dropped
        bus.publish(event).await.unwrap();

        // rx2 still receives
        assert!(rx2.try_recv().is_ok());
    }

    #[tokio::test]
    async fn dead_subscribers_cleaned_on_next_subscribe() {
        let (_dir, bus) = setup().await;
        let rx1 = bus.subscribe().await;
        let _rx2 = bus.subscribe().await;

        // Drop rx1
        drop(rx1);

        // Subscribe again -- should clean up the dead one
        let _rx3 = bus.subscribe().await;

        let subs = bus.subscribers.read().await;
        // rx1 was dropped and cleaned, rx2 and rx3 remain
        assert_eq!(subs.len(), 2);
    }

    #[tokio::test]
    async fn events_arrive_in_order() {
        let (_dir, bus) = setup().await;
        let mut rx = bus.subscribe().await;

        for i in 0..5 {
            let event = Event::new(
                "s1".into(),
                EventKind::LlmCallStarted { iteration: i },
                "agent".into(),
            );
            bus.publish(event).await.unwrap();
        }

        for i in 0..5 {
            let received = rx.try_recv().unwrap();
            if let EventKind::LlmCallStarted { iteration } = received.kind {
                assert_eq!(iteration, i);
            } else {
                panic!("unexpected event kind");
            }
            assert_eq!(received.sequence, (i + 1) as u64);
        }
    }
}
