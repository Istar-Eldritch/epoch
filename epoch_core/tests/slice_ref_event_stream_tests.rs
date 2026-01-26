//! Integration tests for `SliceRefEventStream`.
//!
//! These tests verify that:
//! 1. `SliceRefEventStream` yields references to events without cloning
//! 2. `re_hydrate_from_refs` correctly processes reference-based streams
//! 3. The aggregate's internal use of `SliceRefEventStream` works correctly

use epoch_core::event::{EnumConversionError, Event, EventData};
use epoch_core::prelude::*;
use epoch_mem::*;
use std::pin::Pin;
use tokio_stream::StreamExt;
use uuid::Uuid;

// ============================================================================
// Test Event Types
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum TestEvent {
    Created { name: String },
    Updated { name: String },
}

impl EventData for TestEvent {
    fn event_type(&self) -> &'static str {
        match self {
            TestEvent::Created { .. } => "Created",
            TestEvent::Updated { .. } => "Updated",
        }
    }
}

// Identity conversion for testing
impl TryFrom<&TestEvent> for TestEvent {
    type Error = EnumConversionError;

    fn try_from(value: &TestEvent) -> Result<Self, Self::Error> {
        Ok(value.clone())
    }
}

// ============================================================================
// Test State
// ============================================================================

#[derive(Debug, Clone)]
struct TestState {
    id: Uuid,
    name: String,
    update_count: u32,
}

impl EventApplicatorState for TestState {
    fn get_id(&self) -> &Uuid {
        &self.id
    }
}

// ============================================================================
// Test Projection
// ============================================================================

struct TestProjection {
    state_store: InMemoryStateStore<TestState>,
}

impl TestProjection {
    fn new() -> Self {
        Self {
            state_store: InMemoryStateStore::new(),
        }
    }
}

impl epoch_core::SubscriberId for TestProjection {
    fn subscriber_id(&self) -> &str {
        "projection:test-slice-ref"
    }
}

#[derive(Debug, thiserror::Error)]
enum TestProjectionError {
    #[error("Missing state")]
    MissingState,
}

impl EventApplicator<TestEvent> for TestProjection {
    type State = TestState;
    type StateStore = InMemoryStateStore<TestState>;
    type EventType = TestEvent;
    type ApplyError = TestProjectionError;

    fn get_state_store(&self) -> Self::StateStore {
        self.state_store.clone()
    }

    fn apply(
        &self,
        state: Option<Self::State>,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::ApplyError> {
        match event.data.as_ref().unwrap() {
            TestEvent::Created { name } => Ok(Some(TestState {
                id: event.stream_id,
                name: name.clone(),
                update_count: 0,
            })),
            TestEvent::Updated { name } => {
                let mut state = state.ok_or(TestProjectionError::MissingState)?;
                state.name = name.clone();
                state.update_count += 1;
                Ok(Some(state))
            }
        }
    }
}

impl Projection<TestEvent> for TestProjection {}

// ============================================================================
// Tests for SliceRefEventStream
// ============================================================================

/// Test that SliceRefEventStream yields references to events
#[tokio::test]
async fn slice_ref_event_stream_yields_references() {
    let stream_id = Uuid::new_v4();
    let events = vec![
        TestEvent::Created {
            name: "Test".to_string(),
        }
        .into_builder()
        .stream_id(stream_id)
        .stream_version(1)
        .build()
        .unwrap(),
        TestEvent::Updated {
            name: "Updated".to_string(),
        }
        .into_builder()
        .stream_id(stream_id)
        .stream_version(2)
        .build()
        .unwrap(),
    ];

    let mut stream =
        SliceRefEventStream::<TestEvent, std::convert::Infallible>::from(events.as_slice());
    let mut stream = Pin::new(&mut stream);

    // First event
    let event1 = stream.next().await.unwrap().unwrap();
    assert_eq!(event1.stream_version, 1);
    assert!(matches!(event1.data, Some(TestEvent::Created { .. })));

    // Second event
    let event2 = stream.next().await.unwrap().unwrap();
    assert_eq!(event2.stream_version, 2);
    assert!(matches!(event2.data, Some(TestEvent::Updated { .. })));

    // Stream exhausted
    assert!(stream.next().await.is_none());
}

/// Test that SliceRefEventStream can be used with re_hydrate_from_refs
#[tokio::test]
async fn re_hydrate_from_refs_processes_reference_stream() {
    let stream_id = Uuid::new_v4();
    let events = vec![
        TestEvent::Created {
            name: "Initial".to_string(),
        }
        .into_builder()
        .stream_id(stream_id)
        .stream_version(1)
        .build()
        .unwrap(),
        TestEvent::Updated {
            name: "First Update".to_string(),
        }
        .into_builder()
        .stream_id(stream_id)
        .stream_version(2)
        .build()
        .unwrap(),
        TestEvent::Updated {
            name: "Second Update".to_string(),
        }
        .into_builder()
        .stream_id(stream_id)
        .stream_version(3)
        .build()
        .unwrap(),
    ];

    let projection = TestProjection::new();
    let event_stream = Box::pin(
        SliceRefEventStream::<TestEvent, std::convert::Infallible>::from(events.as_slice()),
    );

    let state = projection
        .re_hydrate_from_refs::<std::convert::Infallible>(None, event_stream)
        .await
        .unwrap()
        .expect("State should exist after processing events");

    assert_eq!(state.id, stream_id);
    assert_eq!(state.name, "Second Update");
    assert_eq!(state.update_count, 2); // Two updates applied
}

/// Test that SliceRefEventStream with empty slice returns None immediately
#[tokio::test]
async fn slice_ref_event_stream_handles_empty_slice() {
    let events: Vec<Event<TestEvent>> = vec![];
    let mut stream =
        SliceRefEventStream::<TestEvent, std::convert::Infallible>::from(events.as_slice());
    let mut stream = Pin::new(&mut stream);

    assert!(stream.next().await.is_none());
}

/// Test that re_hydrate_from_refs with empty stream preserves initial state
#[tokio::test]
async fn re_hydrate_from_refs_preserves_initial_state_on_empty_stream() {
    let stream_id = Uuid::new_v4();
    let initial_state = TestState {
        id: stream_id,
        name: "Preserved".to_string(),
        update_count: 5,
    };

    let events: Vec<Event<TestEvent>> = vec![];
    let projection = TestProjection::new();
    let event_stream = Box::pin(
        SliceRefEventStream::<TestEvent, std::convert::Infallible>::from(events.as_slice()),
    );

    let state = projection
        .re_hydrate_from_refs::<std::convert::Infallible>(Some(initial_state.clone()), event_stream)
        .await
        .unwrap()
        .expect("State should be preserved");

    assert_eq!(state.id, stream_id);
    assert_eq!(state.name, "Preserved");
    assert_eq!(state.update_count, 5);
}

/// Test that re_hydrate_from_refs builds state from scratch when starting with None
#[tokio::test]
async fn re_hydrate_from_refs_builds_state_from_scratch() {
    let stream_id = Uuid::new_v4();
    let events = vec![
        TestEvent::Created {
            name: "FromScratch".to_string(),
        }
        .into_builder()
        .stream_id(stream_id)
        .stream_version(1)
        .build()
        .unwrap(),
    ];

    let projection = TestProjection::new();
    let event_stream = Box::pin(
        SliceRefEventStream::<TestEvent, std::convert::Infallible>::from(events.as_slice()),
    );

    let state = projection
        .re_hydrate_from_refs::<std::convert::Infallible>(None, event_stream)
        .await
        .unwrap()
        .expect("State should be created");

    assert_eq!(state.id, stream_id);
    assert_eq!(state.name, "FromScratch");
    assert_eq!(state.update_count, 0);
}

/// Test that SliceRefEventStream correctly implements RefEventStream trait
#[tokio::test]
async fn slice_ref_event_stream_implements_ref_event_stream_trait() {
    let stream_id = Uuid::new_v4();
    let events = vec![
        TestEvent::Created {
            name: "Test".to_string(),
        }
        .into_builder()
        .stream_id(stream_id)
        .stream_version(1)
        .build()
        .unwrap(),
    ];

    // This function accepts any RefEventStream, proving SliceRefEventStream implements it
    async fn process_ref_stream<'a, E: std::error::Error + Send + Sync + 'a>(
        mut stream: Pin<Box<dyn RefEventStream<'a, TestEvent, E> + Send + 'a>>,
    ) -> Option<u64> {
        stream
            .next()
            .await
            .and_then(|r| r.ok().map(|e| e.stream_version))
    }

    let stream: Pin<Box<dyn RefEventStream<'_, TestEvent, std::convert::Infallible> + Send>> =
        Box::pin(SliceRefEventStream::from(events.as_slice()));

    let version = process_ref_stream(stream).await;
    assert_eq!(version, Some(1));
}
