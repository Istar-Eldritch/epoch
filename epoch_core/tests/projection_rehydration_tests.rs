//! Integration tests for projection re-hydration and concurrency behavior.
//!
//! These tests verify that:
//! 1. `re_hydrate_until()` correctly stops before a specified version
//! 2. `apply_and_store` correctly re-hydrates when state is stale
//! 3. Concurrent event processing doesn't cause lost updates
//! 4. Version tracking works correctly across multiple events
//! 5. Edge cases around state gaps are handled properly

use epoch_core::event::EnumConversionError;
use epoch_core::prelude::*;
use epoch_mem::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Barrier;
use uuid::Uuid;

// ============================================================================
// Test Event Types
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum CounterEvent {
    Created,
    Incremented,
}

impl EventData for CounterEvent {
    fn event_type(&self) -> &'static str {
        match self {
            CounterEvent::Created => "CounterCreated",
            CounterEvent::Incremented => "CounterIncremented",
        }
    }
}

impl TryFrom<&CounterEvent> for CounterEvent {
    type Error = EnumConversionError;

    fn try_from(value: &CounterEvent) -> Result<Self, Self::Error> {
        Ok(value.clone())
    }
}

// ============================================================================
// Test State
// ============================================================================

#[derive(Debug, Clone)]
struct CounterState {
    id: Uuid,
    value: u64,
    version: u64,
}

impl EventApplicatorState for CounterState {
    fn get_id(&self) -> Uuid {
        self.id
    }
}

impl ProjectionState for CounterState {
    fn get_version(&self) -> u64 {
        self.version
    }

    fn set_version(&mut self, version: u64) {
        self.version = version;
    }
}

// ============================================================================
// Test Projection
// ============================================================================

struct CounterProjection {
    state_store: InMemoryStateStore<CounterState>,
    event_store: InMemoryEventStore<InMemoryEventBus<CounterEvent>>,
    apply_count: Arc<AtomicU64>,
}

impl CounterProjection {
    fn new(event_store: InMemoryEventStore<InMemoryEventBus<CounterEvent>>) -> Self {
        Self {
            state_store: InMemoryStateStore::new(),
            event_store,
            apply_count: Arc::new(AtomicU64::new(0)),
        }
    }

    fn with_state_store(
        event_store: InMemoryEventStore<InMemoryEventBus<CounterEvent>>,
        state_store: InMemoryStateStore<CounterState>,
    ) -> Self {
        Self {
            state_store,
            event_store,
            apply_count: Arc::new(AtomicU64::new(0)),
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum CounterProjectionError {
    #[error("Cannot increment without state")]
    NoState,
}

impl EventApplicator<CounterEvent> for CounterProjection {
    type State = CounterState;
    type StateStore = InMemoryStateStore<CounterState>;
    type EventType = CounterEvent;
    type ApplyError = CounterProjectionError;

    fn get_state_store(&self) -> Self::StateStore {
        self.state_store.clone()
    }

    fn apply(
        &self,
        state: Option<Self::State>,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::ApplyError> {
        self.apply_count.fetch_add(1, Ordering::SeqCst);

        match event.data.as_ref().unwrap() {
            CounterEvent::Created => Ok(Some(CounterState {
                id: event.stream_id,
                value: 0,
                version: event.stream_version,
            })),
            CounterEvent::Incremented => {
                let mut state = state.ok_or(CounterProjectionError::NoState)?;
                state.value += 1;
                state.version = event.stream_version;
                Ok(Some(state))
            }
        }
    }
}

impl Projection<CounterEvent> for CounterProjection {
    type EventStore = InMemoryEventStore<InMemoryEventBus<CounterEvent>>;

    fn get_event_store(&self) -> Self::EventStore {
        self.event_store.clone()
    }
}

// ============================================================================
// Helper Functions
// ============================================================================

fn create_event(stream_id: Uuid, version: u64) -> Event<CounterEvent> {
    CounterEvent::Created
        .into_builder()
        .stream_id(stream_id)
        .stream_version(version)
        .build()
        .unwrap()
}

fn increment_event(stream_id: Uuid, version: u64) -> Event<CounterEvent> {
    CounterEvent::Incremented
        .into_builder()
        .stream_id(stream_id)
        .stream_version(version)
        .build()
        .unwrap()
}

// ============================================================================
// Tests for re_hydrate_until()
// ============================================================================

/// Test that re_hydrate_until stops before the specified version
#[tokio::test]
async fn re_hydrate_until_stops_before_specified_version() {
    let bus = InMemoryEventBus::<CounterEvent>::new();
    let event_store = InMemoryEventStore::new(bus);
    let projection = CounterProjection::new(event_store.clone());

    let stream_id = Uuid::new_v4();

    // Store events v1, v2, v3, v4
    event_store
        .store_event(create_event(stream_id, 1))
        .await
        .unwrap();
    event_store
        .store_event(increment_event(stream_id, 2))
        .await
        .unwrap();
    event_store
        .store_event(increment_event(stream_id, 3))
        .await
        .unwrap();
    event_store
        .store_event(increment_event(stream_id, 4))
        .await
        .unwrap();

    // Re-hydrate until version 3 (should apply v1, v2 only)
    let stream = event_store.read_events(stream_id).await.unwrap();
    let state = projection
        .re_hydrate_until::<InMemoryEventStoreBackendError>(None, stream, 3)
        .await
        .unwrap();

    let state = state.expect("State should exist");
    assert_eq!(state.value, 1); // Created (0) + 1 increment = 1
    assert_eq!(state.version, 2); // Last applied was v2
}

/// Test that re_hydrate_until with version 1 applies nothing
#[tokio::test]
async fn re_hydrate_until_version_1_applies_nothing() {
    let bus = InMemoryEventBus::<CounterEvent>::new();
    let event_store = InMemoryEventStore::new(bus);
    let projection = CounterProjection::new(event_store.clone());

    let stream_id = Uuid::new_v4();

    // Store events
    event_store
        .store_event(create_event(stream_id, 1))
        .await
        .unwrap();
    event_store
        .store_event(increment_event(stream_id, 2))
        .await
        .unwrap();

    // Re-hydrate until version 1 (should apply nothing)
    let stream = event_store.read_events(stream_id).await.unwrap();
    let state = projection
        .re_hydrate_until::<InMemoryEventStoreBackendError>(None, stream, 1)
        .await
        .unwrap();

    assert!(state.is_none());
}

/// Test that re_hydrate_until with existing state continues correctly
#[tokio::test]
async fn re_hydrate_until_continues_from_existing_state() {
    let bus = InMemoryEventBus::<CounterEvent>::new();
    let event_store = InMemoryEventStore::new(bus);
    let projection = CounterProjection::new(event_store.clone());

    let stream_id = Uuid::new_v4();

    // Store events v1, v2, v3, v4
    event_store
        .store_event(create_event(stream_id, 1))
        .await
        .unwrap();
    event_store
        .store_event(increment_event(stream_id, 2))
        .await
        .unwrap();
    event_store
        .store_event(increment_event(stream_id, 3))
        .await
        .unwrap();
    event_store
        .store_event(increment_event(stream_id, 4))
        .await
        .unwrap();

    // Start with existing state at v2
    let initial_state = CounterState {
        id: stream_id,
        value: 1, // After create + 1 increment
        version: 2,
    };

    // Re-hydrate from v3 until v4 (should apply v3 only)
    let stream = event_store.read_events_since(stream_id, 3).await.unwrap();
    let state = projection
        .re_hydrate_until::<InMemoryEventStoreBackendError>(Some(initial_state), stream, 4)
        .await
        .unwrap();

    let state = state.expect("State should exist");
    assert_eq!(state.value, 2); // 1 + 1 increment = 2
    assert_eq!(state.version, 3); // Last applied was v3
}

// ============================================================================
// Tests for apply_and_store Re-hydration
// ============================================================================

/// Test that apply_and_store re-hydrates when state is stale
#[tokio::test]
async fn apply_and_store_rehydrates_when_state_is_stale() {
    let bus = InMemoryEventBus::<CounterEvent>::new();
    let event_store = InMemoryEventStore::new(bus);
    let state_store = InMemoryStateStore::new();
    let projection = CounterProjection::with_state_store(event_store.clone(), state_store.clone());

    let stream_id = Uuid::new_v4();

    // Store initial events without triggering projection
    event_store
        .store_event(create_event(stream_id, 1))
        .await
        .unwrap();
    event_store
        .store_event(increment_event(stream_id, 2))
        .await
        .unwrap();

    // Manually set stale state (version 1, value 0)
    let stale_state = CounterState {
        id: stream_id,
        value: 0,
        version: 1,
    };
    state_store
        .clone()
        .persist_state(stream_id, stale_state)
        .await
        .unwrap();

    // Store event v3
    event_store
        .store_event(increment_event(stream_id, 3))
        .await
        .unwrap();

    // Apply event v3 via apply_and_store
    // Should re-hydrate v2 first, then apply v3
    let event_v3 = increment_event(stream_id, 3);
    projection.apply_and_store(&event_v3).await.unwrap();

    let state = state_store.get_state(stream_id).await.unwrap().unwrap();
    assert_eq!(state.value, 2); // 0 + increment(v2) + increment(v3) = 2
    assert_eq!(state.version, 3);
}

/// Test that apply_and_store skips re-hydration when state is current
#[tokio::test]
async fn apply_and_store_skips_rehydration_when_state_is_current() {
    let bus = InMemoryEventBus::<CounterEvent>::new();
    let event_store = InMemoryEventStore::new(bus);
    let state_store = InMemoryStateStore::new();
    let projection = CounterProjection::with_state_store(event_store.clone(), state_store.clone());

    let stream_id = Uuid::new_v4();

    // Store event v1
    event_store
        .store_event(create_event(stream_id, 1))
        .await
        .unwrap();

    // Set current state (version 1)
    let current_state = CounterState {
        id: stream_id,
        value: 0,
        version: 1,
    };
    state_store
        .clone()
        .persist_state(stream_id, current_state)
        .await
        .unwrap();

    // Store event v2
    event_store
        .store_event(increment_event(stream_id, 2))
        .await
        .unwrap();

    // Reset apply count
    projection.apply_count.store(0, Ordering::SeqCst);

    // Apply event v2 - state is at v1, event is v2, no gap
    let event_v2 = increment_event(stream_id, 2);
    projection.apply_and_store(&event_v2).await.unwrap();

    // Should only apply once (no re-hydration needed)
    assert_eq!(projection.apply_count.load(Ordering::SeqCst), 1);

    let state = state_store.get_state(stream_id).await.unwrap().unwrap();
    assert_eq!(state.value, 1);
    assert_eq!(state.version, 2);
}

/// Test apply_and_store with no prior state and first event
#[tokio::test]
async fn apply_and_store_handles_first_event_with_no_state() {
    let bus = InMemoryEventBus::<CounterEvent>::new();
    let event_store = InMemoryEventStore::new(bus);
    let state_store = InMemoryStateStore::new();
    let projection = CounterProjection::with_state_store(event_store.clone(), state_store.clone());

    let stream_id = Uuid::new_v4();

    // Store event v1
    event_store
        .store_event(create_event(stream_id, 1))
        .await
        .unwrap();

    // Apply event v1 with no prior state
    let event_v1 = create_event(stream_id, 1);
    projection.apply_and_store(&event_v1).await.unwrap();

    let state = state_store.get_state(stream_id).await.unwrap().unwrap();
    assert_eq!(state.value, 0);
    assert_eq!(state.version, 1);
}

/// Test apply_and_store with no prior state but event is not v1 (gap scenario)
#[tokio::test]
async fn apply_and_store_rehydrates_from_scratch_when_no_state_and_gap() {
    let bus = InMemoryEventBus::<CounterEvent>::new();
    let event_store = InMemoryEventStore::new(bus);
    let state_store = InMemoryStateStore::new();
    let projection = CounterProjection::with_state_store(event_store.clone(), state_store.clone());

    let stream_id = Uuid::new_v4();

    // Store events v1, v2, v3
    event_store
        .store_event(create_event(stream_id, 1))
        .await
        .unwrap();
    event_store
        .store_event(increment_event(stream_id, 2))
        .await
        .unwrap();
    event_store
        .store_event(increment_event(stream_id, 3))
        .await
        .unwrap();

    // Apply event v3 with no prior state
    // Should re-hydrate v1, v2 first, then apply v3
    let event_v3 = increment_event(stream_id, 3);
    projection.apply_and_store(&event_v3).await.unwrap();

    let state = state_store.get_state(stream_id).await.unwrap().unwrap();
    assert_eq!(state.value, 2); // create + 2 increments
    assert_eq!(state.version, 3);
}

// ============================================================================
// Tests for Concurrent Event Processing
// ============================================================================

/// Test that concurrent events for the same entity don't cause lost updates
///
/// This is the critical race condition test. We simulate:
/// - Two events (v2, v3) arriving concurrently for the same stream
/// - Both starting with state at v1
/// - Without re-hydration, one update would be lost
#[tokio::test]
async fn concurrent_events_no_lost_updates() {
    let bus = InMemoryEventBus::<CounterEvent>::new();
    let event_store = InMemoryEventStore::new(bus);
    let state_store = InMemoryStateStore::new();

    let stream_id = Uuid::new_v4();

    // Store initial events
    event_store
        .store_event(create_event(stream_id, 1))
        .await
        .unwrap();
    event_store
        .store_event(increment_event(stream_id, 2))
        .await
        .unwrap();
    event_store
        .store_event(increment_event(stream_id, 3))
        .await
        .unwrap();

    // Set state at v1 (stale)
    let stale_state = CounterState {
        id: stream_id,
        value: 0,
        version: 1,
    };
    state_store
        .clone()
        .persist_state(stream_id, stale_state)
        .await
        .unwrap();

    // Create two projections sharing the same stores
    let projection1 =
        CounterProjection::with_state_store(event_store.clone(), state_store.clone());
    let projection2 =
        CounterProjection::with_state_store(event_store.clone(), state_store.clone());

    let event_v2 = increment_event(stream_id, 2);
    let event_v3 = increment_event(stream_id, 3);

    // Use a barrier to synchronize concurrent execution
    let barrier = Arc::new(Barrier::new(2));
    let barrier1 = barrier.clone();
    let barrier2 = barrier.clone();

    // Spawn two tasks that will process events concurrently
    let handle1 = tokio::spawn(async move {
        barrier1.wait().await;
        projection1.apply_and_store(&event_v2).await
    });

    let handle2 = tokio::spawn(async move {
        barrier2.wait().await;
        projection2.apply_and_store(&event_v3).await
    });

    // Wait for both to complete
    let (result1, result2) = tokio::join!(handle1, handle2);
    result1.unwrap().unwrap();
    result2.unwrap().unwrap();

    // Final state should have both increments applied
    // The order may vary, but final value should be correct
    let state = state_store.get_state(stream_id).await.unwrap().unwrap();

    // Both events should be reflected in the final state
    // Due to re-hydration, whichever finishes last will have the correct final state
    // The key assertion: value should be 2 (not 1, which would indicate a lost update)
    assert!(
        state.value >= 2,
        "Expected value >= 2, got {}. Lost update detected!",
        state.value
    );
}

/// Test concurrent processing with many events
#[tokio::test]
async fn concurrent_many_events_no_lost_updates() {
    let bus = InMemoryEventBus::<CounterEvent>::new();
    let event_store = InMemoryEventStore::new(bus);
    let state_store = InMemoryStateStore::new();

    let stream_id = Uuid::new_v4();
    let num_events = 10;

    // Store create event
    event_store
        .store_event(create_event(stream_id, 1))
        .await
        .unwrap();

    // Store increment events
    for version in 2..=(num_events as u64 + 1) {
        event_store
            .store_event(increment_event(stream_id, version))
            .await
            .unwrap();
    }

    // Set state at v1 (stale)
    let stale_state = CounterState {
        id: stream_id,
        value: 0,
        version: 1,
    };
    state_store
        .clone()
        .persist_state(stream_id, stale_state)
        .await
        .unwrap();

    // Process all events concurrently
    let barrier = Arc::new(Barrier::new(num_events));
    let mut handles = vec![];

    for version in 2..=(num_events as u64 + 1) {
        let event_store = event_store.clone();
        let state_store = state_store.clone();
        let barrier = barrier.clone();
        let event = increment_event(stream_id, version);

        let handle = tokio::spawn(async move {
            let projection = CounterProjection::with_state_store(event_store, state_store);
            barrier.wait().await;
            projection.apply_and_store(&event).await
        });
        handles.push(handle);
    }

    // Wait for all to complete
    for handle in handles {
        handle.await.unwrap().unwrap();
    }

    // Final state should reflect all events
    let state = state_store.get_state(stream_id).await.unwrap().unwrap();
    assert!(
        state.value >= num_events as u64,
        "Expected value >= {}, got {}. Lost updates detected!",
        num_events,
        state.value
    );
}

// ============================================================================
// Tests for Version Tracking
// ============================================================================

/// Test that version is correctly tracked across sequential events via bus subscription
#[tokio::test]
async fn version_tracking_across_sequential_events() {
    let bus = InMemoryEventBus::<CounterEvent>::new();
    let event_store = InMemoryEventStore::new(bus.clone());
    let state_store = InMemoryStateStore::new();

    let projection = CounterProjection::with_state_store(event_store.clone(), state_store.clone());

    // Subscribe projection to bus
    bus.subscribe(ProjectionHandler::new(projection))
        .await
        .unwrap();

    let stream_id = Uuid::new_v4();

    // Store events sequentially - projection receives them via bus
    event_store
        .store_event(create_event(stream_id, 1))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    let state = state_store.get_state(stream_id).await.unwrap().unwrap();
    assert_eq!(state.version, 1, "Version should be 1 after first event");

    event_store
        .store_event(increment_event(stream_id, 2))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    let state = state_store.get_state(stream_id).await.unwrap().unwrap();
    assert_eq!(state.version, 2, "Version should be 2 after second event");

    event_store
        .store_event(increment_event(stream_id, 3))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    let state = state_store.get_state(stream_id).await.unwrap().unwrap();
    assert_eq!(state.version, 3, "Version should be 3 after third event");
    assert_eq!(state.value, 2, "Value should be 2 after create + 2 increments");
}

/// Test version tracking with shared state store
#[tokio::test]
async fn version_tracking_with_shared_state_store() {
    let bus = InMemoryEventBus::<CounterEvent>::new();
    let event_store = InMemoryEventStore::new(bus.clone());
    let state_store = InMemoryStateStore::new();

    let projection = CounterProjection::with_state_store(event_store.clone(), state_store.clone());
    bus.subscribe(ProjectionHandler::new(projection))
        .await
        .unwrap();

    let stream_id = Uuid::new_v4();

    // Store events and let projection process them
    event_store
        .store_event(create_event(stream_id, 1))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    let state = state_store.get_state(stream_id).await.unwrap().unwrap();
    assert_eq!(state.version, 1);
    assert_eq!(state.value, 0);

    event_store
        .store_event(increment_event(stream_id, 2))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    let state = state_store.get_state(stream_id).await.unwrap().unwrap();
    assert_eq!(state.version, 2);
    assert_eq!(state.value, 1);

    event_store
        .store_event(increment_event(stream_id, 3))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    let state = state_store.get_state(stream_id).await.unwrap().unwrap();
    assert_eq!(state.version, 3);
    assert_eq!(state.value, 2);
}

// ============================================================================
// Edge Case Tests
// ============================================================================

/// Test that applying an event with version equal to state version + 1 works
#[tokio::test]
async fn apply_event_immediately_after_state_version() {
    let bus = InMemoryEventBus::<CounterEvent>::new();
    let event_store = InMemoryEventStore::new(bus);
    let state_store = InMemoryStateStore::new();
    let projection = CounterProjection::with_state_store(event_store.clone(), state_store.clone());

    let stream_id = Uuid::new_v4();

    // Store event v1
    event_store
        .store_event(create_event(stream_id, 1))
        .await
        .unwrap();

    // Set state at v1
    let state = CounterState {
        id: stream_id,
        value: 0,
        version: 1,
    };
    state_store
        .clone()
        .persist_state(stream_id, state)
        .await
        .unwrap();

    // Store and apply event v2 (immediately after v1)
    event_store
        .store_event(increment_event(stream_id, 2))
        .await
        .unwrap();

    let event_v2 = increment_event(stream_id, 2);
    projection.apply_and_store(&event_v2).await.unwrap();

    let state = state_store.get_state(stream_id).await.unwrap().unwrap();
    assert_eq!(state.version, 2);
    assert_eq!(state.value, 1);
}

/// Test handling of large version gaps
#[tokio::test]
async fn apply_event_with_large_version_gap() {
    let bus = InMemoryEventBus::<CounterEvent>::new();
    let event_store = InMemoryEventStore::new(bus);
    let state_store = InMemoryStateStore::new();
    let projection = CounterProjection::with_state_store(event_store.clone(), state_store.clone());

    let stream_id = Uuid::new_v4();

    // Store events v1 through v100
    event_store
        .store_event(create_event(stream_id, 1))
        .await
        .unwrap();
    for version in 2..=100 {
        event_store
            .store_event(increment_event(stream_id, version))
            .await
            .unwrap();
    }

    // Set state at v1 (large gap to v100)
    let state = CounterState {
        id: stream_id,
        value: 0,
        version: 1,
    };
    state_store
        .clone()
        .persist_state(stream_id, state)
        .await
        .unwrap();

    // Apply event v100
    let event_v100 = increment_event(stream_id, 100);
    projection.apply_and_store(&event_v100).await.unwrap();

    let state = state_store.get_state(stream_id).await.unwrap().unwrap();
    assert_eq!(state.version, 100);
    assert_eq!(state.value, 99); // 99 increments total
}

/// Test multiple streams don't interfere with each other
#[tokio::test]
async fn multiple_streams_isolation() {
    let bus = InMemoryEventBus::<CounterEvent>::new();
    let event_store = InMemoryEventStore::new(bus);
    let state_store = InMemoryStateStore::new();
    let projection = CounterProjection::with_state_store(event_store.clone(), state_store.clone());

    let stream_a = Uuid::new_v4();
    let stream_b = Uuid::new_v4();

    // Store events for stream A
    event_store
        .store_event(create_event(stream_a, 1))
        .await
        .unwrap();
    event_store
        .store_event(increment_event(stream_a, 2))
        .await
        .unwrap();
    event_store
        .store_event(increment_event(stream_a, 3))
        .await
        .unwrap();

    // Store events for stream B
    event_store
        .store_event(create_event(stream_b, 1))
        .await
        .unwrap();
    event_store
        .store_event(increment_event(stream_b, 2))
        .await
        .unwrap();

    // Apply events for both streams
    projection
        .apply_and_store(&create_event(stream_a, 1))
        .await
        .unwrap();
    projection
        .apply_and_store(&increment_event(stream_a, 2))
        .await
        .unwrap();
    projection
        .apply_and_store(&increment_event(stream_a, 3))
        .await
        .unwrap();

    projection
        .apply_and_store(&create_event(stream_b, 1))
        .await
        .unwrap();
    projection
        .apply_and_store(&increment_event(stream_b, 2))
        .await
        .unwrap();

    // Verify stream A
    let state_a = state_store.get_state(stream_a).await.unwrap().unwrap();
    assert_eq!(state_a.version, 3);
    assert_eq!(state_a.value, 2);

    // Verify stream B
    let state_b = state_store.get_state(stream_b).await.unwrap().unwrap();
    assert_eq!(state_b.version, 2);
    assert_eq!(state_b.value, 1);
}
