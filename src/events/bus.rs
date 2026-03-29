use std::sync::Arc;

use tokio::sync::{mpsc, RwLock};

use super::store::EventStore;
use super::{Event, EventStoreError};

/// Default capacity for subscriber channels. Provides backpressure:
/// if a subscriber falls behind by this many events, new sends will fail
/// (the event is still persisted in the store — subscriber can replay later).
const DEFAULT_CHANNEL_CAPACITY: usize = 256;

/// In-process event bus. Persists events to the store, then fans out to subscribers.
///
/// Backpressure: subscriber channels are bounded. If a subscriber's buffer is full,
/// the event is dropped for that subscriber (but always persisted in the store).
/// Subscribers that fall behind can recover by replaying from the store.
pub struct EventBus {
    store: Arc<dyn EventStore>,
    subscribers: RwLock<Vec<mpsc::Sender<Event>>>,
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
    ///
    /// Events are always persisted even if no subscriber is available or all buffers are full.
    pub async fn publish(&self, mut event: Event) -> Result<(), EventStoreError> {
        self.store.append(&mut event).await?;

        let subs = self.subscribers.read().await;
        for tx in subs.iter() {
            // try_send: non-blocking, drops the event for this subscriber if buffer is full.
            // The event is still in the store — subscriber can replay if needed.
            let _ = tx.try_send(event.clone());
        }
        Ok(())
    }

    /// Subscribe to all events. Returns a bounded receiver.
    ///
    /// If the subscriber falls behind by more than `DEFAULT_CHANNEL_CAPACITY` events,
    /// newer events will be dropped for this subscriber. The subscriber can recover
    /// by reading from the store directly.
    pub async fn subscribe(&self) -> mpsc::Receiver<Event> {
        self.subscribe_with_capacity(DEFAULT_CHANNEL_CAPACITY).await
    }

    /// Subscribe with a custom channel capacity.
    pub async fn subscribe_with_capacity(&self, capacity: usize) -> mpsc::Receiver<Event> {
        let (tx, rx) = mpsc::channel(capacity);
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
                EventKind::LlmCallStarted { iteration: i, message_count: 0, approx_context_chars: 0 },
                "agent".into(),
            );
            bus.publish(event).await.unwrap();
        }

        for i in 0..5 {
            let received = rx.try_recv().unwrap();
            if let EventKind::LlmCallStarted { iteration, .. } = received.kind {
                assert_eq!(iteration, i);
            } else {
                panic!("unexpected event kind");
            }
            assert_eq!(received.sequence, (i + 1) as u64);
        }
    }

    #[tokio::test]
    async fn backpressure_drops_events_for_slow_subscriber() {
        let (_dir, bus) = setup().await;
        // Small capacity to test backpressure
        let mut rx = bus.subscribe_with_capacity(2).await;

        // Publish 5 events — only first 2 should fit in the channel
        for i in 0..5 {
            let event = Event::new(
                "s1".into(),
                EventKind::LlmCallStarted { iteration: i, message_count: 0, approx_context_chars: 0 },
                "agent".into(),
            );
            bus.publish(event).await.unwrap();
        }

        // Subscriber gets first 2
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_ok());
        // 3rd would have been dropped due to full buffer
        // But all 5 are in the store
        let stored = bus.store().read_stream("s1", 1).await.unwrap();
        assert_eq!(stored.len(), 5);
    }
}
