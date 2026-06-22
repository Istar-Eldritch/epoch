//! Regression test: proves [`epoch_core::testing::verify_store_events_atomicity`] *detects*
//! non-atomic `store_events()` implementations.
//!
//! A [`NonAtomicBackend`] is defined here whose `store_events` is the naive per-event loop
//! (identical to the old default implementation that this spec removes). The contract
//! helper is expected to **panic** when run against it, which the `#[should_panic]`
//! attribute converts into a passing test.

use async_trait::async_trait;
use epoch_core::event::{Event, EventData};
use epoch_core::event_store::{EventStoreBackend, EventStream};
use futures_core::Stream;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::Mutex;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Minimal event type
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct TestEvent {
    value: String,
}

impl EventData for TestEvent {
    fn event_type(&self) -> &'static str {
        "TestEvent"
    }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
enum NonAtomicBackendError {
    #[error("stream_version mismatch: expected {expected}, got {actual}")]
    VersionMismatch { expected: u64, actual: u64 },
}

// ---------------------------------------------------------------------------
// Minimal owned event stream
// ---------------------------------------------------------------------------

struct OwnedEventStream<D, E>
where
    D: EventData,
    E: std::error::Error,
{
    events: Vec<Event<D>>,
    index: usize,
    _phantom: std::marker::PhantomData<E>,
}

impl<D, E> OwnedEventStream<D, E>
where
    D: EventData,
    E: std::error::Error,
{
    fn new(events: Vec<Event<D>>) -> Self {
        Self {
            events,
            index: 0,
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<D, E> Unpin for OwnedEventStream<D, E>
where
    D: EventData,
    E: std::error::Error,
{
}

impl<D, E> Stream for OwnedEventStream<D, E>
where
    D: EventData + Send + Sync,
    E: std::error::Error + Send + Sync,
{
    type Item = Result<Event<D>, E>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        if this.index < this.events.len() {
            let event = this.events[this.index].clone();
            this.index += 1;
            Poll::Ready(Some(Ok(event)))
        } else {
            Poll::Ready(None)
        }
    }
}

impl<D, E> EventStream<D, E> for OwnedEventStream<D, E>
where
    D: EventData + Send + Sync,
    E: std::error::Error + Send + Sync,
{
}

// ---------------------------------------------------------------------------
// NonAtomicBackend — the intentionally broken backend
// ---------------------------------------------------------------------------

struct NonAtomicState {
    /// All events per stream, in append order.
    events: HashMap<Uuid, Vec<Event<TestEvent>>>,
    /// Maps stream_id → next expected stream_version.
    versions: HashMap<Uuid, u64>,
}

struct NonAtomicBackend {
    state: Arc<Mutex<NonAtomicState>>,
}

impl NonAtomicBackend {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(NonAtomicState {
                events: HashMap::new(),
                versions: HashMap::new(),
            })),
        }
    }
}

#[async_trait]
impl EventStoreBackend for NonAtomicBackend {
    type Error = NonAtomicBackendError;
    type EventType = TestEvent;

    async fn read_events_range(
        &self,
        stream_id: Uuid,
        from: Option<u64>,
        to: Option<u64>,
    ) -> Result<Pin<Box<dyn EventStream<Self::EventType, Self::Error> + Send + 'life0>>, Self::Error>
    {
        let state = self.state.lock().await;
        let events: Vec<Event<TestEvent>> = state
            .events
            .get(&stream_id)
            .map(|v| {
                v.iter()
                    .filter(|e| from.is_none_or(|f| e.stream_version >= f))
                    .filter(|e| to.is_none_or(|t| e.stream_version <= t))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        drop(state);

        let stream: Pin<Box<dyn EventStream<TestEvent, NonAtomicBackendError> + Send>> = Box::pin(
            OwnedEventStream::<TestEvent, NonAtomicBackendError>::new(events),
        );
        Ok(stream)
    }

    /// Persists a single event, enforcing per-event `stream_version` monotonicity.
    async fn store_event(&self, event: Event<Self::EventType>) -> Result<(), Self::Error> {
        let mut state = self.state.lock().await;
        let expected = *state.versions.get(&event.stream_id).unwrap_or(&1);
        if event.stream_version != expected {
            return Err(NonAtomicBackendError::VersionMismatch {
                expected,
                actual: event.stream_version,
            });
        }
        state.versions.insert(event.stream_id, expected + 1);
        state.events.entry(event.stream_id).or_default().push(event);
        Ok(())
    }

    /// **Non-atomic** `store_events` — the naive per-event loop (the old default's footgun).
    ///
    /// This does NOT roll back events that were already persisted when a later event
    /// in the batch fails, intentionally reproducing the broken behaviour that
    /// [`verify_store_events_atomicity`](epoch_core::testing::verify_store_events_atomicity)
    /// is designed to detect.
    async fn store_events(&self, events: Vec<Event<Self::EventType>>) -> Result<(), Self::Error> {
        for event in events {
            self.store_event(event).await?;
        }
        Ok(())
    }

    async fn read_events_by_correlation_id(
        &self,
        _correlation_id: Uuid,
    ) -> Result<Vec<Event<Self::EventType>>, Self::Error> {
        Ok(vec![])
    }

    async fn trace_causation_chain(
        &self,
        _event_id: Uuid,
    ) -> Result<Vec<Event<Self::EventType>>, Self::Error> {
        Ok(vec![])
    }
}

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

/// Verifies that [`verify_store_events_atomicity`] detects a non-atomic backend.
///
/// The [`NonAtomicBackend`] above uses the old naive per-event loop, which allows
/// partial writes (v2 and v3 are committed before the duplicate v3 is rejected).
/// The helper therefore panics with the expected message, which `#[should_panic]`
/// converts into a passing test — providing permanent regression protection.
#[tokio::test]
#[should_panic(expected = "after a rejected batch the stream must contain only the seeded event")]
async fn verify_store_events_atomicity_detects_non_atomic_backend() {
    let backend = NonAtomicBackend::new();
    epoch_core::testing::verify_store_events_atomicity(backend, |stream_id, stream_version| {
        Event::<TestEvent>::builder()
            .stream_id(stream_id)
            .stream_version(stream_version)
            .event_type("TestEvent".to_string())
            .data(Some(TestEvent {
                value: format!("v{stream_version}"),
            }))
            .build()
            .unwrap()
    })
    .await;
}
