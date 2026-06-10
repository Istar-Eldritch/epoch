//! Tests for the default implementation of [`EventStoreBackend::read_last_event`].
//!
//! These tests verify that:
//! 1. A backend that does **not** override `read_last_event` still compiles and inherits
//!    the default implementation (SC-4).
//! 2. The default implementation drains the stream and returns the final event.
//! 3. An empty stream yields `Ok(None)` (FR-2).

use async_trait::async_trait;
use epoch_core::event::{Event, EventData};
use epoch_core::prelude::*;
use std::pin::Pin;
use uuid::Uuid;

// ============================================================================
// Test Event Type
// ============================================================================

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
enum TestEvent {
    SomethingHappened,
}

impl EventData for TestEvent {
    fn event_type(&self) -> &'static str {
        "SomethingHappened"
    }
}

// ============================================================================
// Test Error Type
// ============================================================================

#[derive(Debug, thiserror::Error)]
#[error("test backend error")]
struct TestError;

// ============================================================================
// Minimal Backend Double
// ============================================================================
//
// This double implements `EventStoreBackend` using only the methods that existed
// *before* `read_last_event` was introduced. It deliberately does **not** override
// `read_last_event`, which is itself the SC-4 assertion: the default implementation
// makes the new method available to every existing backend without changes.

struct MinimalBackend {
    events: Vec<Event<TestEvent>>,
}

#[async_trait]
impl EventStoreBackend for MinimalBackend {
    type EventType = TestEvent;
    type Error = TestError;

    async fn read_events(
        &self,
        stream_id: Uuid,
    ) -> Result<Pin<Box<dyn EventStream<Self::EventType, Self::Error> + Send + 'life0>>, Self::Error>
    {
        self.read_events_since(stream_id, 1).await
    }

    async fn read_events_since(
        &self,
        _stream_id: Uuid,
        _version: u64,
    ) -> Result<Pin<Box<dyn EventStream<Self::EventType, Self::Error> + Send + 'life0>>, Self::Error>
    {
        let stream: SliceEventStream<'_, Self::EventType, Self::Error> =
            self.events.as_slice().into();
        Ok(Box::pin(stream))
    }

    async fn store_event(&self, _event: Event<Self::EventType>) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn read_events_by_correlation_id(
        &self,
        _correlation_id: Uuid,
    ) -> Result<Vec<Event<Self::EventType>>, Self::Error> {
        Ok(Vec::new())
    }

    async fn trace_causation_chain(
        &self,
        _event_id: Uuid,
    ) -> Result<Vec<Event<Self::EventType>>, Self::Error> {
        Ok(Vec::new())
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn make_event(stream_id: Uuid, version: u64) -> Event<TestEvent> {
    Event::<TestEvent> {
        id: Uuid::new_v4(),
        stream_id,
        stream_version: version,
        event_type: "SomethingHappened".to_string(),
        actor_id: None,
        purger_id: None,
        data: Some(TestEvent::SomethingHappened),
        created_at: chrono::Utc::now(),
        purged_at: None,
        global_sequence: Some(version),
        causation_id: None,
        correlation_id: None,
    }
}

// ============================================================================
// Tests
// ============================================================================

#[tokio::test]
async fn read_last_event_returns_final_element_of_multi_event_stream() {
    let stream_id = Uuid::new_v4();
    let v1 = make_event(stream_id, 1);
    let v2 = make_event(stream_id, 2);
    let v3 = make_event(stream_id, 3);

    let backend = MinimalBackend {
        events: vec![v1, v2, v3.clone()],
    };

    let last = backend
        .read_last_event(stream_id)
        .await
        .expect("read_last_event should succeed");

    assert_eq!(last, Some(v3));
}

#[tokio::test]
async fn read_last_event_returns_none_for_empty_stream() {
    let backend = MinimalBackend { events: Vec::new() };

    let last = backend
        .read_last_event(Uuid::new_v4())
        .await
        .expect("read_last_event should succeed");

    assert_eq!(last, None);
}
