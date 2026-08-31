mod common;

use async_trait::async_trait;
use epoch_core::prelude::*;
use epoch_core::projection::ProjectionHandler;
use epoch_derive::EventData;
use epoch_mem::InMemoryStateStore;
use epoch_pg::Migrator;
use epoch_pg::PgDBEvent;
use epoch_pg::event_bus::PgEventBus;
use epoch_pg::event_store::PgEventStore;
use serial_test::serial;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, EventData)]
enum TestEventData {
    TestEvent { value: String },
}

// Identity conversion for testing - clones the data
impl TryFrom<&TestEventData> for TestEventData {
    type Error = epoch_core::event::EnumConversionError;

    fn try_from(value: &TestEventData) -> Result<Self, Self::Error> {
        Ok(value.clone())
    }
}

// Helper function to create a new event
fn new_event(stream_id: Uuid, stream_version: u64, value: &str) -> Event<TestEventData> {
    Event::<TestEventData>::builder()
        .stream_id(stream_id)
        .event_type("MyEvent".to_string())
        .stream_version(stream_version)
        .data(Some(TestEventData::TestEvent {
            value: value.to_string(),
        }))
        .build()
        .unwrap()
}

#[derive(Debug, Clone)]
struct TestState(Vec<Event<TestEventData>>);

impl EventApplicatorState for TestState {
    fn get_id(&self) -> &Uuid {
        // For testing purposes, return a static UUID reference
        static TEST_UUID: std::sync::OnceLock<Uuid> = std::sync::OnceLock::new();
        TEST_UUID.get_or_init(Uuid::new_v4)
    }
}

/// Single stable LISTEN channel for this binary's buses.
///
/// The NOTIFY trigger's name is derived from the channel, so a fresh random
/// channel per test would leave a fresh trigger behind on the shared
/// `epoch_events` table on every run: those accumulate, and every INSERT then
/// fires all of them. A stable name keeps it at one trigger no matter how often
/// the suite runs. Buses sharing a channel is harmless -- they already share the
/// table, and each subscriber only processes events above its own checkpoint.
const TEST_CHANNEL: &str = "test_channel_shared";

struct TestProjection {
    state_store: InMemoryStateStore<TestState>,
    subscriber_id: String,
    subscription_mode: SubscriptionMode,
    failure_mode: FailureMode,
}

impl TestProjection {
    pub fn new() -> Self {
        Self::with_subscriber_id(format!("projection:test:{}", Uuid::new_v4()))
    }

    pub fn with_subscriber_id(subscriber_id: String) -> Self {
        TestProjection {
            state_store: InMemoryStateStore::new(),
            subscriber_id,
            subscription_mode: SubscriptionMode::Checkpointed,
            failure_mode: FailureMode::FailOpen,
        }
    }

    /// A ReplayAlways projection: replays from 0 every process start and never
    /// reads or writes a persisted checkpoint (R5).
    pub fn replay_always(subscriber_id: String) -> Self {
        TestProjection {
            state_store: InMemoryStateStore::new(),
            subscriber_id,
            subscription_mode: SubscriptionMode::ReplayAlways,
            failure_mode: FailureMode::FailOpen,
        }
    }

    /// Marks this projection [`FailureMode::FailClosed`] (spec 0028): it halts
    /// rather than skipping an event it cannot apply in order.
    pub fn fail_closed(mut self) -> Self {
        self.failure_mode = FailureMode::FailClosed;
        self
    }
}

impl epoch_core::SubscriberId for TestProjection {
    fn subscriber_id(&self) -> &str {
        &self.subscriber_id
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TestProjectionError {}

impl EventApplicator<TestEventData> for TestProjection {
    type State = TestState;
    type StateStore = InMemoryStateStore<Self::State>;
    type EventType = TestEventData;
    type ApplyError = TestProjectionError;

    fn get_state_store(&self) -> Self::StateStore {
        self.state_store.clone()
    }
    fn apply(
        &self,
        state: Option<Self::State>,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::ApplyError> {
        if let Some(mut state) = state {
            state.0.push(event.clone());
            Ok(Some(state))
        } else {
            Ok(Some(TestState(vec![event.clone()])))
        }
    }
}

impl Projection<TestEventData> for TestProjection {
    fn subscription_mode(&self) -> SubscriptionMode {
        self.subscription_mode
    }

    fn failure_mode(&self) -> FailureMode {
        self.failure_mode
    }
}

async fn setup() -> Option<(
    PgPool,
    PgEventBus<TestEventData>,
    PgEventStore<PgEventBus<TestEventData>>,
)> {
    common::init_test_logger();
    let pool = common::try_get_pg_pool().await?;

    // Run migrations to set up the schema
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    common::truncate_epoch_tables(&pool).await;

    let channel_name = TEST_CHANNEL.to_string();
    let event_bus = PgEventBus::new(pool.clone(), channel_name);
    let event_store = PgEventStore::new(pool.clone(), event_bus.clone());

    // Set up event bus trigger and start listener
    event_bus
        .setup_trigger()
        .await
        .expect("Failed to setup event bus trigger");
    event_bus
        .start_listener()
        .await
        .expect("Failed to start event bus listener");

    Some((pool, event_bus, event_store))
}

#[tokio::test]
#[serial]
async fn test_setup_with_migrations() {
    let Some((_pool, _event_bus, _event_store)) = setup().await else {
        return;
    };
    // If setup completes without panicking, migrations and event bus setup was successful
}

#[tokio::test]
#[serial]
async fn test_subscribe_and_event_propagation() {
    let Some((_pool, event_bus, event_store)) = setup().await else {
        return;
    };

    let projection = TestProjection::new();
    let projection_events = projection.get_state_store().clone();
    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe projection");

    let stream_id = Uuid::new_v4();
    let event = new_event(stream_id, 1, "test_value");

    // Give some time for the notification to be processed
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Store the event using PgEventStore, which should trigger the NOTIFY
    event_store
        .store_event(event.clone())
        .await
        .expect("Failed to store event");

    // Give some time for the notification to be processed
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let events_received = projection_events
        .get_state(stream_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(events_received.0.len(), 1);
    assert_eq!(events_received.0[0].id, event.id);
    assert_eq!(events_received.0[0].stream_id, event.stream_id);
    assert_eq!(events_received.0[0].stream_version, event.stream_version);
    assert_eq!(events_received.0[0].event_type, event.event_type);
    assert_eq!(events_received.0[0].data, event.data);
}

#[tokio::test]
#[serial]
async fn test_noop_publish() {
    let Some((pool, event_bus, _event_store)) = setup().await else {
        return;
    };

    let projection = TestProjection::new();
    let projection_events = projection.get_state_store().clone();
    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe projection");

    let stream_id = Uuid::new_v4();
    let event = new_event(stream_id, 1, "test_value_noop");

    // Publish the event directly to the event bus (should be a no-op)
    event_bus.publish(Arc::new(event.clone())).await.unwrap();

    // Give some time for the notification to be processed (even though it shouldn't happen)
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Assert that no event was received by the projection
    let events_received = projection_events.get_state(stream_id).await.unwrap();
    assert!(events_received.is_none());

    // Assert that no event was stored in the database
    let db_events: Vec<PgDBEvent> = sqlx::query_as("SELECT * FROM epoch_events WHERE id = $1")
        .bind(event.id)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(db_events.is_empty());
}

#[tokio::test]
#[serial]
async fn test_event_data_deserialization_failure() {
    let Some((pool, event_bus, _event_store)) = setup().await else {
        return;
    };

    let projection = TestProjection::new();
    let projection_events = projection.get_state_store().clone();
    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe projection");

    let stream_id = Uuid::new_v4();

    // Event with valid data
    let valid_event = new_event(stream_id, 1, "valid_data");

    // Store the valid event directly to trigger notification
    sqlx::query(
        r#"
        INSERT INTO epoch_events (id, stream_id, stream_version, event_type, data, created_at, actor_id, purger_id, purged_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        "#,
    )
    .bind(valid_event.id)
    .bind(valid_event.stream_id)
    .bind(valid_event.stream_version as i64)
    .bind(valid_event.event_type.to_string())
    .bind(serde_json::to_value(&valid_event.data).unwrap())
    .bind(valid_event.created_at)
    .bind(valid_event.actor_id)
    .bind(valid_event.purger_id)
    .bind(valid_event.purged_at)
    .execute(&pool)
    .await
    .expect("Failed to insert valid event");

    // Malformed event data (e.g., missing a required field, or wrong type)
    let malformed_event_id = Uuid::new_v4();
    let malformed_event_data = serde_json::json!({ "invalid_field": 123 }); // Malformed data
    sqlx::query(
        r#"
        INSERT INTO epoch_events (id, stream_id, stream_version, event_type, data, created_at)
        VALUES ($1, $2, $3, $4, $5, $6)
        "#,
    )
    .bind(malformed_event_id)
    .bind(stream_id)
    .bind(2i64)
    .bind("TestEvent".to_string())
    .bind(malformed_event_data)
    .bind(chrono::Utc::now())
    .execute(&pool)
    .await
    .expect("Failed to insert malformed event");

    // Another valid event to ensure bus continues processing
    let another_valid_event = new_event(stream_id, 3, "another_valid_data");
    sqlx::query(
        r#"
        INSERT INTO epoch_events (id, stream_id, stream_version, event_type, data, created_at, actor_id, purger_id, purged_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        "#,
    )
    .bind(another_valid_event.id)
    .bind(another_valid_event.stream_id)
    .bind(another_valid_event.stream_version as i64)
    .bind(another_valid_event.event_type.to_string())
    .bind(serde_json::to_value(&another_valid_event.data).unwrap())
    .bind(another_valid_event.created_at)
    .bind(another_valid_event.actor_id)
    .bind(another_valid_event.purger_id)
    .bind(another_valid_event.purged_at)
    .execute(&pool)
    .await
    .expect("Failed to insert another valid event");

    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await; // Give time for processing

    let events_received = projection_events
        .get_state(stream_id)
        .await
        .unwrap()
        .unwrap();

    // Assert that only the valid events were processed
    assert_eq!(events_received.0.len(), 2);
    assert!(events_received.0.iter().any(|e| e.id == valid_event.id));
    assert!(
        events_received
            .0
            .iter()
            .any(|e| e.id == another_valid_event.id)
    );
    assert!(!events_received.0.iter().any(|e| e.id == malformed_event_id));

    // Clean up malformed event to avoid affecting other tests
    sqlx::query("DELETE FROM epoch_events WHERE id = $1")
        .bind(malformed_event_id)
        .execute(&pool)
        .await
        .expect("Failed to clean up malformed event");
}

#[tokio::test]
#[serial]
async fn test_multiple_subscribers() {
    let Some((_pool, event_bus, event_store)) = setup().await else {
        return;
    };

    let projection1 = TestProjection::new();
    let projection_events1 = projection1.get_state_store().clone();
    event_bus
        .subscribe(ProjectionHandler::new(projection1))
        .await
        .expect("Failed to subscribe projection 1");

    let projection2 = TestProjection::new();
    let projection_events2 = projection2.get_state_store().clone();
    event_bus
        .subscribe(ProjectionHandler::new(projection2))
        .await
        .expect("Failed to subscribe projection 2");

    let stream_id = Uuid::new_v4();
    let event = new_event(stream_id, 1, "test_value_multi");

    // Give some time for the notification to be processed
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Store the event using PgEventStore, which should trigger the NOTIFY
    event_store
        .store_event(event.clone())
        .await
        .expect("Failed to store event");

    // Give some time for the notification to be processed
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let events_received1 = projection_events1
        .get_state(stream_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(events_received1.0.len(), 1);
    assert_eq!(events_received1.0[0].id, event.id);

    let events_received2 = projection_events2
        .get_state(stream_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(events_received2.0.len(), 1);
    assert_eq!(events_received2.0[0].id, event.id);
}

#[tokio::test]
#[serial]
async fn test_event_bus_notification_includes_global_sequence() {
    let Some((_pool, event_bus, event_store)) = setup().await else {
        return;
    };

    let projection = TestProjection::new();
    let projection_events = projection.get_state_store().clone();
    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe projection");

    let stream_id = Uuid::new_v4();
    let event = new_event(stream_id, 1, "test_global_sequence");

    // Give some time for the subscription to be ready
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Store the event - this should trigger NOTIFY with global_sequence
    event_store
        .store_event(event.clone())
        .await
        .expect("Failed to store event");

    // Give some time for the notification to be processed
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    let events_received = projection_events
        .get_state(stream_id)
        .await
        .unwrap()
        .expect("Should have received events");

    assert_eq!(events_received.0.len(), 1);

    // Verify that the received event has a global_sequence
    let received_event = &events_received.0[0];
    assert!(
        received_event.global_sequence.is_some(),
        "Event received via event bus should have global_sequence set"
    );
}

#[tokio::test]
#[serial]
async fn test_migrations_create_checkpoint_table() {
    let Some((pool, event_bus, _event_store)) = setup().await else {
        return;
    };

    // Verify that the checkpoint table exists
    let result: (i64,) = sqlx::query_as(
        r#"
        SELECT COUNT(*) 
        FROM information_schema.tables 
        WHERE table_name = 'epoch_event_bus_checkpoints'
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("Failed to query information_schema");

    assert_eq!(
        result.0, 1,
        "epoch_event_bus_checkpoints table should exist"
    );

    // Also verify the table has the expected columns
    let columns: Vec<(String,)> = sqlx::query_as(
        r#"
        SELECT column_name::text
        FROM information_schema.columns 
        WHERE table_name = 'epoch_event_bus_checkpoints'
        ORDER BY ordinal_position
        "#,
    )
    .fetch_all(&pool)
    .await
    .expect("Failed to query columns");

    let column_names: Vec<&str> = columns.iter().map(|(name,)| name.as_str()).collect();
    assert!(column_names.contains(&"subscriber_id"));
    assert!(column_names.contains(&"last_global_sequence"));
    assert!(column_names.contains(&"last_event_id"));
    assert!(column_names.contains(&"updated_at"));

    drop(event_bus);
}

#[tokio::test]
#[serial]
async fn test_checkpoint_read_returns_none_for_new_subscriber() {
    let Some((_pool, event_bus, _event_store)) = setup().await else {
        return;
    };

    let checkpoint = event_bus
        .get_checkpoint("projection:nonexistent")
        .await
        .expect("Should not error");

    assert!(
        checkpoint.is_none(),
        "Checkpoint for new subscriber should be None"
    );

    drop(event_bus);
}

#[tokio::test]
#[serial]
async fn test_checkpoint_write_and_read_roundtrip() {
    let Some((_pool, event_bus, _event_store)) = setup().await else {
        return;
    };

    let subscriber_id = "projection:test-roundtrip";
    let global_sequence = 42u64;
    let event_id = Uuid::new_v4();

    // Write checkpoint
    event_bus
        .update_checkpoint(subscriber_id, global_sequence, event_id)
        .await
        .expect("Should write checkpoint");

    // Read it back
    let checkpoint = event_bus
        .get_checkpoint(subscriber_id)
        .await
        .expect("Should read checkpoint");

    assert_eq!(
        checkpoint,
        Some(global_sequence),
        "Checkpoint should match written value"
    );

    drop(event_bus);
}

#[tokio::test]
#[serial]
async fn test_checkpoint_update_is_upsert() {
    let Some((_pool, event_bus, _event_store)) = setup().await else {
        return;
    };

    let subscriber_id = "projection:test-upsert";
    let event_id1 = Uuid::new_v4();
    let event_id2 = Uuid::new_v4();

    // First write
    event_bus
        .update_checkpoint(subscriber_id, 10, event_id1)
        .await
        .expect("Should write first checkpoint");

    let checkpoint1 = event_bus
        .get_checkpoint(subscriber_id)
        .await
        .expect("Should read first checkpoint");
    assert_eq!(checkpoint1, Some(10));

    // Update with higher value
    event_bus
        .update_checkpoint(subscriber_id, 25, event_id2)
        .await
        .expect("Should update checkpoint");

    let checkpoint2 = event_bus
        .get_checkpoint(subscriber_id)
        .await
        .expect("Should read updated checkpoint");
    assert_eq!(checkpoint2, Some(25));

    drop(event_bus);
}

#[tokio::test]
#[serial]
async fn test_checkpoint_updated_after_successful_event_processing() {
    let Some((_pool, event_bus, event_store)) = setup().await else {
        return;
    };

    let projection = TestProjection::new();
    let subscriber_id = projection.subscriber_id().to_string();

    // Verify no checkpoint exists before subscribing for this unique subscriber
    let initial_checkpoint = event_bus
        .get_checkpoint(&subscriber_id)
        .await
        .expect("Should read checkpoint");
    assert!(
        initial_checkpoint.is_none(),
        "Initial checkpoint should be None before subscribing"
    );

    // Get the current max sequence to know where we started
    let events_before = event_bus
        .read_all_events_since(0, 10000)
        .await
        .expect("Should read events");
    let baseline_seq = events_before
        .last()
        .and_then(|e| e.global_sequence)
        .unwrap_or(0);

    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe projection");

    // Give some time for the subscription to be ready (and catch-up to complete)
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Store a new event (after subscribing)
    let stream_id = Uuid::new_v4();
    let event = new_event(stream_id, 1, "test_checkpoint");

    event_store
        .store_event(event.clone())
        .await
        .expect("Failed to store event");

    // Wait for processing
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    // Verify checkpoint was updated to reflect the new event
    let checkpoint = event_bus
        .get_checkpoint(&subscriber_id)
        .await
        .expect("Should read checkpoint");

    assert!(
        checkpoint.is_some(),
        "Checkpoint should be set after event processing"
    );

    // The checkpoint should be at least the new event's sequence (greater than baseline)
    assert!(
        checkpoint.unwrap() > baseline_seq,
        "Checkpoint should be updated beyond the baseline"
    );
}

// ==================== Phase 4: Catch-up Tests ====================

#[tokio::test]
#[serial]
async fn test_read_all_events_since_returns_events_after_sequence() {
    let Some((_pool, event_bus, event_store)) = setup().await else {
        return;
    };

    // Get current max sequence to establish baseline
    let baseline_events = event_bus
        .read_all_events_since(0, 10000)
        .await
        .expect("Should read events");
    let baseline_seq = baseline_events
        .last()
        .and_then(|e| e.global_sequence)
        .unwrap_or(0);

    // Store 5 events across different streams
    let stream_id1 = Uuid::new_v4();
    let stream_id2 = Uuid::new_v4();

    for i in 1..=3 {
        let event = new_event(stream_id1, i, &format!("stream1_event{}", i));
        event_store
            .store_event(event)
            .await
            .expect("Failed to store event");
    }
    for i in 1..=2 {
        let event = new_event(stream_id2, i, &format!("stream2_event{}", i));
        event_store
            .store_event(event)
            .await
            .expect("Failed to store event");
    }

    // Read events since baseline (our 5 new events)
    let new_events = event_bus
        .read_all_events_since(baseline_seq, 100)
        .await
        .expect("Should read events");
    assert_eq!(new_events.len(), 5, "Should have 5 new events");

    // Verify they are ordered by global_sequence
    for i in 1..new_events.len() {
        assert!(
            new_events[i].global_sequence > new_events[i - 1].global_sequence,
            "Events should be ordered by global_sequence"
        );
    }

    // Get the global_sequence of the 2nd new event
    let seq_2 = new_events[1].global_sequence.unwrap();

    // Read events since sequence 2
    let events_after_2 = event_bus
        .read_all_events_since(seq_2, 100)
        .await
        .expect("Should read events");
    assert_eq!(
        events_after_2.len(),
        3,
        "Should have 3 events after sequence 2"
    );

    // Verify all returned events have global_sequence > seq_2
    for event in &events_after_2 {
        assert!(
            event.global_sequence.unwrap() > seq_2,
            "All events should have global_sequence > {}",
            seq_2
        );
    }

    drop(event_bus);
}

#[tokio::test]
#[serial]
async fn test_read_all_events_since_with_baseline_returns_new_events() {
    let Some((_pool, event_bus, event_store)) = setup().await else {
        return;
    };

    // Get current max sequence to establish baseline
    let baseline_events = event_bus
        .read_all_events_since(0, 10000)
        .await
        .expect("Should read events");
    let baseline_seq = baseline_events
        .last()
        .and_then(|e| e.global_sequence)
        .unwrap_or(0);

    // Store 3 events
    let stream_id = Uuid::new_v4();
    for i in 1..=3 {
        let event = new_event(stream_id, i, &format!("event{}", i));
        event_store
            .store_event(event)
            .await
            .expect("Failed to store event");
    }

    // Read since baseline should return all 3 new events
    let events = event_bus
        .read_all_events_since(baseline_seq, 100)
        .await
        .expect("Should read events");
    assert_eq!(events.len(), 3, "Should return all 3 new events");

    drop(event_bus);
}

#[tokio::test]
#[serial]
async fn test_subscribe_catches_up_on_missed_events() {
    let Some((_pool, event_bus, event_store)) = setup().await else {
        return;
    };

    // Store 3 events BEFORE subscribing
    let stream_id = Uuid::new_v4();
    for i in 1..=3 {
        let event = new_event(stream_id, i, &format!("pre_subscribe_event{}", i));
        event_store
            .store_event(event)
            .await
            .expect("Failed to store event");
    }

    // Give time for the events to be committed
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Now subscribe projection
    let projection = TestProjection::new();
    let projection_events = projection.get_state_store().clone();

    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe projection");

    // Give some time for catch-up to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Verify projection received all 3 events via catch-up
    let events_received = projection_events
        .get_state(stream_id)
        .await
        .unwrap()
        .expect("Should have received events");

    assert_eq!(
        events_received.0.len(),
        3,
        "Projection should have received all 3 pre-existing events via catch-up"
    );
}

#[tokio::test]
#[serial]
async fn test_subscribe_deduplicates_events_during_catchup() {
    let Some((_pool, event_bus, event_store)) = setup().await else {
        return;
    };

    // Store event 1 before subscribing
    let stream_id = Uuid::new_v4();
    let event1 = new_event(stream_id, 1, "event1");
    event_store
        .store_event(event1.clone())
        .await
        .expect("Failed to store event");

    // Give time for the event to be committed
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Subscribe projection - this will catch up and process event1
    let projection = TestProjection::new();
    let projection_events = projection.get_state_store().clone();

    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe projection");

    // Give time for catch-up to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Verify event1 was processed during catch-up
    let events_after_catchup = projection_events
        .get_state(stream_id)
        .await
        .unwrap()
        .expect("Should have received event");
    assert_eq!(
        events_after_catchup.0.len(),
        1,
        "Should have 1 event after catch-up"
    );

    // Store event 2 after catch-up completes (will come via NOTIFY)
    let event2 = new_event(stream_id, 2, "event2");
    event_store
        .store_event(event2.clone())
        .await
        .expect("Failed to store event 2");

    // Wait for event2 to be processed
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Verify we have exactly 2 events (no duplicates)
    let final_events = projection_events
        .get_state(stream_id)
        .await
        .unwrap()
        .expect("Should have received events");

    assert_eq!(
        final_events.0.len(),
        2,
        "Should have exactly 2 events (event1 from catch-up, event2 from NOTIFY)"
    );
}

#[tokio::test]
#[serial]
async fn test_catchup_with_batching() {
    let Some((pool, event_bus, event_store)) = setup().await else {
        return;
    };

    // Create event bus with small batch size for testing
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        catch_up_batch_size: 5, // Small batch size
        ..Default::default()
    };
    let channel_name = "test_batch_channel".to_string();
    let event_bus_batched =
        epoch_pg::event_bus::PgEventBus::with_config(pool.clone(), channel_name, config);

    // Set up event bus trigger and start listener (migrations already run in setup)
    event_bus_batched
        .setup_trigger()
        .await
        .expect("Failed to setup event bus trigger");
    event_bus_batched
        .start_listener()
        .await
        .expect("Failed to start event bus listener");

    // Store 12 events (will require 3 batches with batch_size=5)
    let stream_id = Uuid::new_v4();
    for i in 1..=12 {
        let event = new_event(stream_id, i, &format!("batch_event{}", i));
        event_store
            .store_event(event)
            .await
            .expect("Failed to store event");
    }

    // Give time for events to be committed
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    // Subscribe - will catch up in batches
    let projection = TestProjection::new();
    let projection_events = projection.get_state_store().clone();

    event_bus_batched
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe projection");

    // Give time for catch-up to complete (multiple batches)
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Verify all 12 events were received
    let events_received = projection_events
        .get_state(stream_id)
        .await
        .unwrap()
        .expect("Should have received events");

    assert_eq!(
        events_received.0.len(),
        12,
        "Projection should have received all 12 events via batched catch-up"
    );

    drop(event_bus);
}

// ==================== Phase 5: Retry & DLQ Tests ====================

#[tokio::test]
#[serial]
async fn test_migrations_create_dlq_table() {
    let Some((pool, _event_bus, _event_store)) = setup().await else {
        return;
    };

    // Verify that the DLQ table exists
    let result: (i64,) = sqlx::query_as(
        r#"
        SELECT COUNT(*) 
        FROM information_schema.tables 
        WHERE table_name = 'epoch_event_bus_dlq'
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("Failed to query information_schema");

    assert_eq!(result.0, 1, "epoch_event_bus_dlq table should exist");

    // Verify the DLQ indexes exist
    let idx_subscriber: (i64,) = sqlx::query_as(
        r#"
        SELECT COUNT(*) 
        FROM pg_indexes 
        WHERE tablename = 'epoch_event_bus_dlq' AND indexname = 'idx_epoch_dlq_subscriber'
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("Failed to query pg_indexes");

    assert_eq!(
        idx_subscriber.0, 1,
        "idx_epoch_dlq_subscriber index should exist"
    );

    let idx_created_at: (i64,) = sqlx::query_as(
        r#"
        SELECT COUNT(*) 
        FROM pg_indexes 
        WHERE tablename = 'epoch_event_bus_dlq' AND indexname = 'idx_epoch_dlq_created_at'
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("Failed to query pg_indexes");

    assert_eq!(
        idx_created_at.0, 1,
        "idx_epoch_dlq_created_at index should exist"
    );
}

#[tokio::test]
#[serial]
async fn test_dlq_insert_and_retrieve() {
    let Some((_pool, event_bus, _event_store)) = setup().await else {
        return;
    };

    let subscriber_id = format!("projection:test-dlq:{}", Uuid::new_v4());
    let event_id = Uuid::new_v4();
    let global_sequence = 42u64;
    let error_message = "Test error message";

    // Insert into DLQ
    event_bus
        .insert_into_dlq(&subscriber_id, event_id, global_sequence, error_message, 3)
        .await
        .expect("Should insert into DLQ");

    // Retrieve DLQ entries
    let entries = event_bus
        .get_dlq_entries(&subscriber_id)
        .await
        .expect("Should get DLQ entries");

    assert_eq!(entries.len(), 1, "Should have 1 DLQ entry");
    assert_eq!(entries[0].subscriber_id, subscriber_id);
    assert_eq!(entries[0].event_id, event_id);
    assert_eq!(entries[0].global_sequence, global_sequence);
    assert_eq!(entries[0].error_message, Some(error_message.to_string()));
    assert_eq!(entries[0].retry_count, 3);
}

#[tokio::test]
#[serial]
async fn test_dlq_upsert_updates_existing_entry() {
    let Some((_pool, event_bus, _event_store)) = setup().await else {
        return;
    };

    let subscriber_id = format!("projection:test-dlq-upsert:{}", Uuid::new_v4());
    let event_id = Uuid::new_v4();

    // First insert
    event_bus
        .insert_into_dlq(&subscriber_id, event_id, 10, "First error", 1)
        .await
        .expect("Should insert into DLQ");

    // Second insert (upsert)
    event_bus
        .insert_into_dlq(&subscriber_id, event_id, 10, "Second error", 2)
        .await
        .expect("Should upsert into DLQ");

    // Verify only one entry exists with updated values
    let entries = event_bus
        .get_dlq_entries(&subscriber_id)
        .await
        .expect("Should get DLQ entries");

    assert_eq!(
        entries.len(),
        1,
        "Should have only 1 DLQ entry after upsert"
    );
    assert_eq!(entries[0].error_message, Some("Second error".to_string()));
    assert_eq!(entries[0].retry_count, 2);
}

// ==================== Phase 6: Multi-Instance Coordination Tests ====================

#[tokio::test]
#[serial]
async fn test_advisory_lock_acquisition() {
    let Some((_pool, event_bus, _event_store)) = setup().await else {
        return;
    };

    let subscriber_id = format!("projection:test-lock:{}", Uuid::new_v4());

    // Acquire lock - should succeed
    let acquired = event_bus
        .try_acquire_subscriber_lock(&subscriber_id)
        .await
        .expect("Should try to acquire lock");

    assert!(acquired, "First lock acquisition should succeed");

    // Note: We can't reliably test release with a connection pool because
    // advisory locks are session-based and the pool may use different connections.
    // The lock will be automatically released when the connection is returned to the pool
    // or closed.
}

#[tokio::test]
#[serial]
async fn test_advisory_lock_can_be_acquired_by_different_subscribers() {
    let Some((_pool, event_bus, _event_store)) = setup().await else {
        return;
    };

    let subscriber_id1 = format!("projection:test-lock-1:{}", Uuid::new_v4());
    let subscriber_id2 = format!("projection:test-lock-2:{}", Uuid::new_v4());

    // Acquire lock for first subscriber
    let first_acquired = event_bus
        .try_acquire_subscriber_lock(&subscriber_id1)
        .await
        .expect("Should try to acquire lock for subscriber 1");

    assert!(
        first_acquired,
        "First subscriber lock acquisition should succeed"
    );

    // Acquire lock for second subscriber - should also succeed since it's a different key
    let second_acquired = event_bus
        .try_acquire_subscriber_lock(&subscriber_id2)
        .await
        .expect("Should try to acquire lock for subscriber 2");

    assert!(
        second_acquired,
        "Second subscriber should be able to acquire its own lock"
    );
}

#[tokio::test]
#[serial]
async fn test_multiple_subscribers_have_independent_checkpoints() {
    let Some((_pool, event_bus, event_store)) = setup().await else {
        return;
    };

    // Create two different projections with different subscriber_ids
    // We'll simulate this by manually updating checkpoints

    let subscriber_1 = "projection:subscriber-1";
    let subscriber_2 = "projection:subscriber-2";

    // Store some events
    let stream_id = Uuid::new_v4();
    let event1 = new_event(stream_id, 1, "event1");
    let event2 = new_event(stream_id, 2, "event2");

    event_store
        .store_event(event1.clone())
        .await
        .expect("Failed to store event");
    event_store
        .store_event(event2.clone())
        .await
        .expect("Failed to store event");

    // Update checkpoint for subscriber 1 to event 1
    event_bus
        .update_checkpoint(subscriber_1, 1, event1.id)
        .await
        .expect("Should update checkpoint");

    // Update checkpoint for subscriber 2 to event 2
    event_bus
        .update_checkpoint(subscriber_2, 2, event2.id)
        .await
        .expect("Should update checkpoint");

    // Verify checkpoints are independent
    let cp1 = event_bus
        .get_checkpoint(subscriber_1)
        .await
        .expect("Should get checkpoint");
    let cp2 = event_bus
        .get_checkpoint(subscriber_2)
        .await
        .expect("Should get checkpoint");

    assert_eq!(cp1, Some(1), "Subscriber 1 checkpoint should be 1");
    assert_eq!(cp2, Some(2), "Subscriber 2 checkpoint should be 2");
}

// ==================== InstanceMode::Coordinated Tests ====================

#[tokio::test]
#[serial]
async fn test_coordinated_mode_acquires_lock_on_subscribe() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };

    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    common::truncate_epoch_tables(&pool).await;

    // Create event bus with coordinated mode
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        instance_mode: epoch_pg::event_bus::InstanceMode::Coordinated,
        ..Default::default()
    };
    let channel_name = "test_coord_channel".to_string();
    let event_bus: PgEventBus<TestEventData> =
        epoch_pg::event_bus::PgEventBus::with_config(pool.clone(), channel_name, config);

    event_bus
        .setup_trigger()
        .await
        .expect("Failed to setup trigger");
    event_bus
        .start_listener()
        .await
        .expect("Failed to start listener");

    // Subscribe a projection - should acquire lock
    let projection = TestProjection::new();
    let subscriber_id = projection.subscriber_id().to_string();

    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe projection");

    // Verify lock was acquired by trying to acquire it from a completely separate connection
    // Use a new pool with max_connections=1 to ensure we get a fresh connection
    let database_url = common::database_url();
    let check_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .expect("Failed to create check pool");

    let lock_result: (bool,) = sqlx::query_as(
        r#"
        SELECT pg_try_advisory_lock(
            ('x' || substr(md5($1), 1, 8))::bit(32)::int,
            ('x' || substr(md5($1), 9, 8))::bit(32)::int
        )
        "#,
    )
    .bind(&subscriber_id)
    .fetch_one(&check_pool)
    .await
    .expect("Failed to check lock");

    // If the lock is held by the event bus, our attempt to acquire should fail (return false)
    assert!(
        !lock_result.0,
        "Lock should be held by the event bus after subscribe in coordinated mode (our attempt to acquire should fail)"
    );

    // Close the check pool
    check_pool.close().await;
}

#[tokio::test]
#[serial]
async fn test_coordinated_mode_skips_subscribe_if_lock_held() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };

    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    common::truncate_epoch_tables(&pool).await;

    // Use a fixed subscriber ID for this test
    let subscriber_id = format!("projection:coord-test:{}", Uuid::new_v4());

    // First, acquire the lock from a separate connection to simulate another instance
    let mut lock_conn = pool.acquire().await.expect("Failed to acquire connection");
    let acquired: (bool,) = sqlx::query_as(
        r#"
        SELECT pg_try_advisory_lock(
            ('x' || substr(md5($1), 1, 8))::bit(32)::int,
            ('x' || substr(md5($1), 9, 8))::bit(32)::int
        )
        "#,
    )
    .bind(&subscriber_id)
    .fetch_one(&mut *lock_conn)
    .await
    .expect("Failed to acquire lock");

    assert!(acquired.0, "Should acquire lock from separate connection");

    // Create event bus with coordinated mode
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        instance_mode: epoch_pg::event_bus::InstanceMode::Coordinated,
        ..Default::default()
    };
    let channel_name = "test_coord_skip_channel".to_string();
    let event_bus: PgEventBus<TestEventData> =
        epoch_pg::event_bus::PgEventBus::with_config(pool.clone(), channel_name, config);

    event_bus
        .setup_trigger()
        .await
        .expect("Failed to setup trigger");
    event_bus
        .start_listener()
        .await
        .expect("Failed to start listener");

    // Store an event before subscribing
    let stream_id = Uuid::new_v4();
    let event = new_event(stream_id, 1, "test_event");
    sqlx::query(
        r#"
        INSERT INTO epoch_events (id, stream_id, stream_version, event_type, data, created_at)
        VALUES ($1, $2, $3, $4, $5, $6)
        "#,
    )
    .bind(event.id)
    .bind(event.stream_id)
    .bind(event.stream_version as i64)
    .bind(&event.event_type)
    .bind(serde_json::to_value(&event.data).unwrap())
    .bind(event.created_at)
    .execute(&pool)
    .await
    .expect("Failed to store event");

    // Subscribe projection with the same subscriber_id - should skip because lock is held
    let projection = TestProjection::with_subscriber_id(subscriber_id.clone());
    let projection_events = projection.get_state_store().clone();

    // Subscribe should succeed (not error), but projection should NOT be registered
    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Subscribe should succeed even when lock is held");

    // Give time for any potential processing
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Verify projection did NOT receive any events (it was skipped)
    let events_received = projection_events.get_state(stream_id).await.unwrap();
    assert!(
        events_received.is_none(),
        "Projection should NOT have received events when lock was already held"
    );

    // A subscriber this instance declined to drive must also stay invisible to
    // readiness. It is not registered, so it has no position here, and the bus-wide
    // gate must not wait on one: it would never advance and would burn the caller's
    // whole timeout on every call.
    assert!(
        matches!(
            event_bus.subscriber_lag(&subscriber_id).await,
            Err(epoch_pg::PgEventBusError::SubscriberNotFound(_))
        ),
        "a Coordinated-mode subscribe that lost the advisory-lock race must not be \
         visible to readiness on this instance"
    );
    assert!(
        event_bus
            .wait_until_all_caught_up(tokio::time::Duration::from_millis(300))
            .await
            .expect("wait_until_all_caught_up failed"),
        "the bus-wide gate must not wait on a subscriber this instance does not drive"
    );

    // Clean up - release the lock
    let _: (bool,) = sqlx::query_as(
        r#"
        SELECT pg_advisory_unlock(
            ('x' || substr(md5($1), 1, 8))::bit(32)::int,
            ('x' || substr(md5($1), 9, 8))::bit(32)::int
        )
        "#,
    )
    .bind(&subscriber_id)
    .fetch_one(&mut *lock_conn)
    .await
    .expect("Failed to release lock");
}

#[tokio::test]
#[serial]
async fn test_coordinated_mode_allows_different_subscribers_on_same_instance() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };

    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    common::truncate_epoch_tables(&pool).await;

    // Create event bus with coordinated mode
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        instance_mode: epoch_pg::event_bus::InstanceMode::Coordinated,
        ..Default::default()
    };
    let channel_name = "test_coord_multi_channel".to_string();
    let event_bus: PgEventBus<TestEventData> =
        epoch_pg::event_bus::PgEventBus::with_config(pool.clone(), channel_name, config);
    let event_store = PgEventStore::new(pool.clone(), event_bus.clone());

    event_bus
        .setup_trigger()
        .await
        .expect("Failed to setup trigger");
    event_bus
        .start_listener()
        .await
        .expect("Failed to start listener");

    // Subscribe two projections with different subscriber_ids
    let projection1 = TestProjection::new(); // Has unique subscriber_id
    let projection_events1 = projection1.get_state_store().clone();

    let projection2 = TestProjection::new(); // Has different unique subscriber_id
    let projection_events2 = projection2.get_state_store().clone();

    event_bus
        .subscribe(ProjectionHandler::new(projection1))
        .await
        .expect("Failed to subscribe projection 1");

    event_bus
        .subscribe(ProjectionHandler::new(projection2))
        .await
        .expect("Failed to subscribe projection 2");

    // Store an event
    let stream_id = Uuid::new_v4();
    let event = new_event(stream_id, 1, "multi_subscriber_event");

    // Give time for subscriptions to be ready
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    event_store
        .store_event(event.clone())
        .await
        .expect("Failed to store event");

    // Give time for processing
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Both projections should receive the event
    let events1 = projection_events1
        .get_state(stream_id)
        .await
        .unwrap()
        .expect("Projection 1 should have received event");
    let events2 = projection_events2
        .get_state(stream_id)
        .await
        .unwrap()
        .expect("Projection 2 should have received event");

    assert_eq!(events1.0.len(), 1, "Projection 1 should have 1 event");
    assert_eq!(events2.0.len(), 1, "Projection 2 should have 1 event");
}

// ==================== CheckpointMode::Batched Tests ====================

/// R3/R4 positive control: `Batched`'s `batch_size` trigger flushes the exact
/// contiguous position after the threshold is crossed.
///
/// Runs on an isolated events table with its own sequence, so the flushed
/// checkpoint can be asserted for **exact** equality against the fifth event's
/// `global_sequence` (the shared table's sequence is unpredictable). Exact
/// equality is what catches the counter regression this control exists for: an
/// `is_some()` assertion passes even if the counter froze and only a later
/// `max_delay`/shutdown flush wrote anything, while an exact match against the
/// batch-boundary sequence only holds if the `batch_size` trigger itself fired.
#[tokio::test]
#[serial]
async fn test_batched_checkpoint_flushes_at_batch_size() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };

    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

    let table = isolated_events_table(&pool).await;

    // Configure batched mode with batch_size=5 and long max_delay (won't trigger),
    // so only the batch_size threshold can flush.
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        checkpoint_mode: epoch_pg::event_bus::CheckpointMode::Batched {
            batch_size: 5,
            max_delay_ms: 60000, // 60 seconds - won't trigger
        },
        events_table: table.clone(),
        ..Default::default()
    };

    let channel_name = format!("test_batched_bs_{}", Uuid::new_v4().simple());
    let event_bus =
        epoch_pg::event_bus::PgEventBus::with_config(pool.clone(), channel_name, config);

    // LIKE does not copy triggers, so the NOTIFY trigger must be created on the
    // isolated table before the listener starts.
    event_bus
        .setup_trigger()
        .await
        .expect("Failed to setup trigger");
    event_bus
        .start_listener()
        .await
        .expect("Failed to start listener");

    // Subscribe a projection with known subscriber_id
    let subscriber_id = format!("projection:batched_test:{}", Uuid::new_v4());
    let projection = TestProjection::with_subscriber_id(subscriber_id.clone());

    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe");

    // Give time for subscription setup
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let stream_id = Uuid::new_v4();

    // Store 4 events - should NOT trigger checkpoint flush yet. Raw inserts on
    // the isolated table (rather than store_event) return the assigned
    // global_sequence so the flush can be asserted exactly.
    for i in 1..=4 {
        insert_committed_event(&pool, &table, stream_id, i, &format!("batched_event_{i}")).await;
    }

    // Store the 5th event - should trigger the batch_size checkpoint flush.
    let (_id5, seq5) = insert_committed_event(&pool, &table, stream_id, 5, "batched_event_5").await;

    // Poll for the exact expected value rather than a fixed sleep: this has a
    // genuine positive edge (checkpoint == Some(seq5)), so a fixed sleep risks
    // flaking red under a loaded shared Postgres.
    let mut checkpoint = None;
    for _ in 0..40 {
        checkpoint = event_bus
            .get_checkpoint(&subscriber_id)
            .await
            .expect("Failed to get checkpoint");
        if checkpoint == Some(seq5 as u64) {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    event_bus.shutdown().await.expect("shutdown");
    drop_isolated_events_table(&pool, &table).await;

    assert_eq!(
        checkpoint,
        Some(seq5 as u64),
        "batch_size flush must persist exactly the fifth event's sequence ({seq5})"
    );
}

/// R3/R4 positive control: `Batched`'s `max_delay_ms` trigger flushes the exact
/// contiguous position on the periodic timer tick, with `batch_size` set high
/// enough that only the delay can fire.
///
/// Isolated table + exact-equality assertion, for the same reason as the
/// `batch_size` control above.
#[tokio::test]
#[serial]
async fn test_batched_checkpoint_flushes_at_max_delay() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };

    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

    let table = isolated_events_table(&pool).await;

    // Configure batched mode with large batch_size (won't trigger) and short max_delay
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        checkpoint_mode: epoch_pg::event_bus::CheckpointMode::Batched {
            batch_size: 1000,  // Won't reach this
            max_delay_ms: 500, // 500ms - will trigger
        },
        events_table: table.clone(),
        ..Default::default()
    };

    let channel_name = format!("test_batched_delay_{}", Uuid::new_v4().simple());
    let event_bus =
        epoch_pg::event_bus::PgEventBus::with_config(pool.clone(), channel_name, config);

    event_bus
        .setup_trigger()
        .await
        .expect("Failed to setup trigger");
    event_bus
        .start_listener()
        .await
        .expect("Failed to start listener");

    // Subscribe a projection with known subscriber_id
    let subscriber_id = format!("projection:batched_delay_test:{}", Uuid::new_v4());
    let projection = TestProjection::with_subscriber_id(subscriber_id.clone());

    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe");

    // Give time for subscription setup
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let stream_id = Uuid::new_v4();

    // Store 3 events - won't reach batch_size. Raw inserts return the sequence.
    let mut last_seq = 0i64;
    for i in 1..=3 {
        let (_id, seq) =
            insert_committed_event(&pool, &table, stream_id, i, &format!("delay_event_{i}")).await;
        last_seq = seq;
    }

    // Poll for the exact expected value rather than a fixed sleep: this has a
    // genuine positive edge (checkpoint == Some(last_seq)), so a fixed sleep
    // risks flaking red under a loaded shared Postgres. flush_interval is a
    // hard-coded 1s and max_delay is 500ms, so the periodic flush lands within
    // ~1.5s; the 4s poll budget gives margin for the timer tick's arbitrary
    // phase.
    let mut checkpoint = None;
    for _ in 0..40 {
        checkpoint = event_bus
            .get_checkpoint(&subscriber_id)
            .await
            .expect("Failed to get checkpoint");
        if checkpoint == Some(last_seq as u64) {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    event_bus.shutdown().await.expect("shutdown");
    drop_isolated_events_table(&pool, &table).await;

    assert_eq!(
        checkpoint,
        Some(last_seq as u64),
        "max_delay flush must persist exactly the last processed sequence ({last_seq})"
    );
}

#[tokio::test]
#[serial]
async fn test_batched_checkpoint_during_catchup() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };

    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    common::truncate_epoch_tables(&pool).await;

    // First, store some events before subscribing (these will be caught up)
    let channel_name = "test_batched_catchup".to_string();

    // Use a separate event bus just for storing events (with default config)
    let store_bus = epoch_pg::event_bus::PgEventBus::new(pool.clone(), channel_name.clone());
    let event_store = PgEventStore::new(pool.clone(), store_bus.clone());

    let stream_id = Uuid::new_v4();

    // Store 12 events before subscribing
    for i in 1..=12 {
        let event = new_event(stream_id, i, &format!("catchup_event_{}", i));
        event_store
            .store_event(event)
            .await
            .expect("Failed to store event");
    }

    // Give time for events to be committed
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Now create event bus with batched checkpointing (batch_size=5)
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        checkpoint_mode: epoch_pg::event_bus::CheckpointMode::Batched {
            batch_size: 5,
            max_delay_ms: 60000, // Won't trigger during catch-up
        },
        catch_up_batch_size: 100, // Large enough to get all events in one batch
        ..Default::default()
    };

    let event_bus =
        epoch_pg::event_bus::PgEventBus::with_config(pool.clone(), channel_name, config);

    event_bus
        .setup_trigger()
        .await
        .expect("Failed to setup trigger");
    event_bus
        .start_listener()
        .await
        .expect("Failed to start listener");

    // Subscribe - will catch up on 12 events
    let subscriber_id = format!("projection:batched_catchup:{}", Uuid::new_v4());
    let projection = TestProjection::with_subscriber_id(subscriber_id.clone());
    let projection_events = projection.get_state_store().clone();

    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe");

    // Give time for catch-up to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Verify all events were received
    let events = projection_events
        .get_state(stream_id)
        .await
        .unwrap()
        .expect("Should have received events");
    assert_eq!(events.0.len(), 12, "All 12 events should be caught up");

    // Checkpoint should be written (final flush after catch-up)
    let checkpoint = event_bus
        .get_checkpoint(&subscriber_id)
        .await
        .expect("Failed to get checkpoint");
    assert!(
        checkpoint.is_some(),
        "Checkpoint should be written after catch-up completes"
    );
}

/// R3 positive control for the default mode: `Synchronous` advances its
/// checkpoint per batch. This is the only control that pins cadence on the
/// default configuration, and the P2 change that moves the flush call out of
/// the advance branch affects `Synchronous` too, so it must be exact-value and
/// span more than one event.
///
/// Three events are stored (rather than one): a single-event version would
/// still catch a total failure to advance (checkpoint would be `None`), but
/// proves nothing about advancing across a batch. Isolated table so the final
/// sequence is exactly the third event's.
#[tokio::test]
#[serial]
async fn test_synchronous_checkpoint_still_works() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };

    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

    let table = isolated_events_table(&pool).await;

    // Explicitly use Synchronous mode (also the default).
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        checkpoint_mode: epoch_pg::event_bus::CheckpointMode::Synchronous,
        events_table: table.clone(),
        ..Default::default()
    };

    let channel_name = format!("test_sync_checkpoint_{}", Uuid::new_v4().simple());
    let event_bus =
        epoch_pg::event_bus::PgEventBus::with_config(pool.clone(), channel_name, config);

    event_bus
        .setup_trigger()
        .await
        .expect("Failed to setup trigger");
    event_bus
        .start_listener()
        .await
        .expect("Failed to start listener");

    let subscriber_id = format!("projection:sync_test:{}", Uuid::new_v4());
    let projection = TestProjection::with_subscriber_id(subscriber_id.clone());

    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe");

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let stream_id = Uuid::new_v4();

    // Store three events; in Synchronous mode the checkpoint advances per batch,
    // so it must end at exactly the third event's sequence.
    let mut last_seq = 0i64;
    for i in 1..=3 {
        let (_id, seq) =
            insert_committed_event(&pool, &table, stream_id, i, &format!("sync_event_{i}")).await;
        last_seq = seq;
    }

    // Poll for the exact expected value rather than a fixed sleep: this has a
    // genuine positive edge (checkpoint == Some(last_seq)), so a fixed sleep
    // risks flaking red under a loaded shared Postgres.
    let mut checkpoint = None;
    for _ in 0..40 {
        checkpoint = event_bus
            .get_checkpoint(&subscriber_id)
            .await
            .expect("Failed to get checkpoint");
        if checkpoint == Some(last_seq as u64) {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    event_bus.shutdown().await.expect("shutdown");
    drop_isolated_events_table(&pool, &table).await;

    assert_eq!(
        checkpoint,
        Some(last_seq as u64),
        "synchronous mode must persist exactly the last processed sequence ({last_seq})"
    );
}

// ============================================================================
// Lifecycle Tests
// ============================================================================

/// Helper to create an event bus without starting the listener
async fn setup_without_listener() -> Option<(PgPool, PgEventBus<TestEventData>)> {
    common::init_test_logger();
    let pool = common::try_get_pg_pool().await?;

    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    common::truncate_epoch_tables(&pool).await;

    let channel_name = TEST_CHANNEL.to_string();
    let event_bus = PgEventBus::new(pool.clone(), channel_name);

    Some((pool, event_bus))
}

#[tokio::test]
#[serial]
async fn test_is_running_returns_false_before_start() {
    let Some((_pool, event_bus)) = setup_without_listener().await else {
        return;
    };

    assert!(
        !event_bus.is_running().await,
        "is_running should return false before start_listener is called"
    );
}

#[tokio::test]
#[serial]
async fn test_is_running_returns_true_after_start() {
    let Some((_pool, event_bus)) = setup_without_listener().await else {
        return;
    };

    event_bus
        .setup_trigger()
        .await
        .expect("Failed to setup trigger");

    event_bus
        .start_listener()
        .await
        .expect("Failed to start listener");

    assert!(
        event_bus.is_running().await,
        "is_running should return true after start_listener is called"
    );

    // Cleanup
    event_bus.shutdown().await.expect("Failed to shutdown");
}

#[tokio::test]
#[serial]
async fn test_shutdown_stops_listener() {
    let Some((_pool, event_bus)) = setup_without_listener().await else {
        return;
    };

    event_bus
        .setup_trigger()
        .await
        .expect("Failed to setup trigger");

    event_bus
        .start_listener()
        .await
        .expect("Failed to start listener");

    assert!(event_bus.is_running().await, "Listener should be running");

    event_bus
        .shutdown()
        .await
        .expect("Failed to shutdown listener");

    assert!(
        !event_bus.is_running().await,
        "is_running should return false after shutdown"
    );
}

#[tokio::test]
#[serial]
async fn test_shutdown_without_start_returns_error() {
    let Some((_pool, event_bus)) = setup_without_listener().await else {
        return;
    };

    let result = event_bus.shutdown().await;
    assert!(
        result.is_err(),
        "shutdown should return error if listener was not started"
    );
}

#[tokio::test]
#[serial]
async fn test_start_listener_is_idempotent() {
    let Some((_pool, event_bus)) = setup_without_listener().await else {
        return;
    };

    event_bus
        .setup_trigger()
        .await
        .expect("Failed to setup trigger");

    // Start listener twice - second call should be a no-op
    event_bus
        .start_listener()
        .await
        .expect("Failed to start listener first time");

    event_bus
        .start_listener()
        .await
        .expect("Second start_listener should succeed (no-op)");

    assert!(
        event_bus.is_running().await,
        "Listener should still be running"
    );

    // Cleanup
    event_bus.shutdown().await.expect("Failed to shutdown");
}

#[tokio::test]
#[serial]
async fn test_shutdown_flushes_batched_checkpoints() {
    use epoch_pg::event_bus::{CheckpointMode, ReliableDeliveryConfig};
    use std::time::Duration;

    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };

    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    common::truncate_epoch_tables(&pool).await;

    // Create event bus with batched checkpointing (large batch size so it won't auto-flush)
    let config = ReliableDeliveryConfig {
        checkpoint_mode: CheckpointMode::Batched {
            batch_size: 1000,
            max_delay_ms: 60000, // 60 seconds - won't trigger during test
        },
        ..Default::default()
    };

    let channel_name = "test_shutdown_flush".to_string();
    let event_bus = PgEventBus::<TestEventData>::with_config(pool.clone(), channel_name, config);
    let event_store = PgEventStore::new(pool.clone(), event_bus.clone());

    event_bus
        .setup_trigger()
        .await
        .expect("Failed to setup trigger");

    event_bus
        .start_listener()
        .await
        .expect("Failed to start listener");

    // Subscribe a projection
    let projection = TestProjection::new();
    let subscriber_id = projection.subscriber_id().to_string();

    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe");

    // Store an event
    let stream_id = Uuid::new_v4();
    let event = new_event(stream_id, 1, "batched_event");
    event_store
        .store_event(event)
        .await
        .expect("Failed to store event");

    // Wait for event to be processed
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Before shutdown, checkpoint might not be flushed (batched mode)
    // After shutdown, it should be flushed
    event_bus
        .shutdown()
        .await
        .expect("Failed to shutdown listener");

    // Verify checkpoint was flushed during shutdown
    let checkpoint = event_bus
        .get_checkpoint(&subscriber_id)
        .await
        .expect("Failed to get checkpoint");

    assert!(
        checkpoint.is_some(),
        "Checkpoint should be flushed during graceful shutdown"
    );
}

// ============================================================================
// Configuration Tests
// ============================================================================

#[tokio::test]
async fn test_catch_up_buffer_size_config() {
    use epoch_pg::event_bus::ReliableDeliveryConfig;

    // Verify default value
    let default_config = ReliableDeliveryConfig::default();
    assert_eq!(
        default_config.catch_up_buffer_size, 10_000,
        "Default catch_up_buffer_size should be 10,000"
    );

    // Verify custom value
    let custom_config = ReliableDeliveryConfig {
        catch_up_buffer_size: 500,
        ..Default::default()
    };
    assert_eq!(
        custom_config.catch_up_buffer_size, 500,
        "Custom catch_up_buffer_size should be respected"
    );
}

// ============================================================================
// Phase 3: Out-of-Order NOTIFY Regression Test
// ============================================================================

/// Reproduces the out-of-order NOTIFY delivery bug described in spec 0012.
///
/// PostgreSQL's `nextval()` is non-transactional: two concurrent transactions
/// can obtain adjacent sequence numbers but commit in reverse order. This causes
/// NOTIFY messages to arrive out-of-order relative to `global_sequence`.
///
/// The old payload-based listener would process event N+1 first and set the
/// checkpoint to N+1, then skip event N when its NOTIFY arrived (N ≤ checkpoint).
///
/// The new DB-query-driven listener fetches committed events in sequence order
/// on each NOTIFY, uses gap tracking to wait for N to become visible, and
/// processes both events correctly.
#[tokio::test]
#[serial]
async fn test_out_of_order_notify_both_events_processed() {
    let Some((pool, event_bus, _event_store)) = setup().await else {
        return;
    };

    let projection = TestProjection::new();
    let projection_events = projection.get_state_store().clone();

    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe projection");

    // Give the subscription time to become active (catch-up and buffer setup)
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let stream_id = Uuid::new_v4();
    let event_a_id = Uuid::new_v4();
    let event_b_id = Uuid::new_v4();

    // Prepare serialized event data in the format the event bus expects.
    let event_a_data = serde_json::to_value(Some(TestEventData::TestEvent {
        value: "event_a".to_string(),
    }))
    .unwrap();
    let event_b_data = serde_json::to_value(Some(TestEventData::TestEvent {
        value: "event_b".to_string(),
    }))
    .unwrap();

    // Task A: insert event A (obtains a lower global_sequence via nextval()),
    // then deliberately delays its COMMIT so that event B commits first.
    //
    // Timeline:
    //   t=0ms   : Task A begins tx, executes INSERT (nextval → seq N)
    //   t=50ms  : Task B begins tx, executes INSERT (nextval → seq N+1), COMMITs
    //             → NOTIFY for seq N+1 fires; event N not yet committed
    //   t=200ms : Task A COMMITs
    //             → NOTIFY for seq N fires; both N and N+1 now visible in DB
    //
    // Old behaviour (bug): listener receives NOTIFY N+1, processes from payload,
    //   sets checkpoint=N+1. On NOTIFY N, sees N ≤ checkpoint → skips event N.
    //
    // New behaviour (fix): listener receives NOTIFY N+1, queries DB (only N+1
    //   visible), processes N+1, detects gap at N. On NOTIFY N, queries DB
    //   (both visible), processes N, advances checkpoint past both.
    let pool_a = pool.clone();
    let task_a = {
        let event_a_data = event_a_data.clone();
        tokio::spawn(async move {
            let mut tx = pool_a.begin().await.expect("tx A: begin");
            sqlx::query(
                r#"
                INSERT INTO epoch_events
                    (id, stream_id, stream_version, event_type, data, created_at)
                VALUES ($1, $2, $3, $4, $5, NOW())
                "#,
            )
            .bind(event_a_id)
            .bind(stream_id)
            .bind(1i64)
            .bind("MyEvent")
            .bind(event_a_data)
            .execute(&mut *tx)
            .await
            .expect("tx A: insert");

            // Hold the transaction open so event B commits first, producing a
            // NOTIFY for N+1 before the NOTIFY for N.
            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

            tx.commit().await.expect("tx A: commit");
        })
    };

    // Task B: insert event B (obtains a higher global_sequence) and commit
    // immediately, so its NOTIFY fires before Task A's NOTIFY.
    let pool_b = pool.clone();
    let task_b = {
        let event_b_data = event_b_data.clone();
        tokio::spawn(async move {
            // Small delay ensures Task A's INSERT has already executed (and
            // consumed a lower sequence number) before Task B inserts.
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

            let mut tx = pool_b.begin().await.expect("tx B: begin");
            sqlx::query(
                r#"
                INSERT INTO epoch_events
                    (id, stream_id, stream_version, event_type, data, created_at)
                VALUES ($1, $2, $3, $4, $5, NOW())
                "#,
            )
            .bind(event_b_id)
            .bind(stream_id)
            .bind(2i64)
            .bind("MyEvent")
            .bind(event_b_data)
            .execute(&mut *tx)
            .await
            .expect("tx B: insert");

            // Commit immediately → NOTIFY for seq N+1 fires before Task A commits
            tx.commit().await.expect("tx B: commit");
        })
    };

    // Wait for both transactions to complete (Task A finishes last at ~200ms)
    task_a.await.expect("Task A panicked");
    task_b.await.expect("Task B panicked");

    // Allow enough time for the listener to receive both NOTIFYs and process them.
    // Task A commits at ~200ms, so we need well beyond that.
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    let events_received = projection_events
        .get_state(stream_id)
        .await
        .unwrap()
        .expect("Should have received at least one event");

    let received_ids: Vec<Uuid> = events_received.0.iter().map(|e| e.id).collect();

    assert_eq!(
        events_received.0.len(),
        2,
        "Both events must be processed despite out-of-order NOTIFY delivery. \
         Received event IDs: {:?}",
        received_ids
    );
    assert!(
        received_ids.contains(&event_a_id),
        "Event A (lower sequence, late commit) must be processed"
    );
    assert!(
        received_ids.contains(&event_b_id),
        "Event B (higher sequence, early commit) must be processed"
    );
}

/// Test that a rolled-back transaction creating a permanent gap in global_sequence
/// is resolved by the periodic timer after gap_timeout expires.
///
/// Scenario:
///   1. Event at seq N commits normally
///   2. A transaction obtains seq N+1 via nextval() but rolls back (permanent gap)
///   3. Event at seq N+2 commits normally
///   4. The periodic timer detects the gap has timed out and advances past it
///   5. Both events N and N+2 are processed
#[tokio::test]
#[serial]
async fn test_rolled_back_transaction_gap_resolved() {
    let Some((pool, _event_bus, _event_store)) = setup().await else {
        return;
    };

    // Create event bus with short gap_timeout for testing
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        gap_timeout: std::time::Duration::from_secs(1),
        ..Default::default()
    };
    let channel_name = "test_gap_resolve".to_string();
    let event_bus = epoch_pg::event_bus::PgEventBus::<TestEventData>::with_config(
        pool.clone(),
        channel_name,
        config,
    );

    event_bus
        .setup_trigger()
        .await
        .expect("Failed to setup event bus trigger");
    event_bus
        .start_listener()
        .await
        .expect("Failed to start listener");

    let projection = TestProjection::new();
    let projection_events = projection.get_state_store().clone();

    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe projection");

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let stream_id = Uuid::new_v4();

    // Event 1: normal commit (seq N)
    let event1_id = Uuid::new_v4();
    let event1_data = serde_json::to_value(Some(TestEventData::TestEvent {
        value: "event_1".to_string(),
    }))
    .unwrap();
    sqlx::query(
        r#"INSERT INTO epoch_events (id, stream_id, stream_version, event_type, data, created_at)
           VALUES ($1, $2, 1, 'MyEvent', $3, NOW())"#,
    )
    .bind(event1_id)
    .bind(stream_id)
    .bind(&event1_data)
    .execute(&pool)
    .await
    .expect("insert event 1");

    // Small delay to let event 1 process
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Rolled-back transaction: obtains seq N+1 but never commits
    {
        let mut tx = pool.begin().await.expect("begin rollback tx");
        sqlx::query(
            r#"INSERT INTO epoch_events (id, stream_id, stream_version, event_type, data, created_at)
               VALUES ($1, $2, 2, 'MyEvent', $3, NOW())"#,
        )
        .bind(Uuid::new_v4())
        .bind(stream_id)
        .bind(&event1_data)
        .execute(&mut *tx)
        .await
        .expect("insert rollback event");
        tx.rollback().await.expect("rollback");
    }

    // Event 3: normal commit (seq N+2, with gap at N+1)
    let event3_id = Uuid::new_v4();
    let event3_data = serde_json::to_value(Some(TestEventData::TestEvent {
        value: "event_3".to_string(),
    }))
    .unwrap();
    sqlx::query(
        r#"INSERT INTO epoch_events (id, stream_id, stream_version, event_type, data, created_at)
           VALUES ($1, $2, 3, 'MyEvent', $3, NOW())"#,
    )
    .bind(event3_id)
    .bind(stream_id)
    .bind(&event3_data)
    .execute(&pool)
    .await
    .expect("insert event 3");

    // Wait for gap_timeout (1s) + periodic tick (1s) + processing buffer
    tokio::time::sleep(tokio::time::Duration::from_millis(3500)).await;

    let events_received = projection_events
        .get_state(stream_id)
        .await
        .unwrap()
        .expect("Should have received events");

    let received_ids: Vec<Uuid> = events_received.0.iter().map(|e| e.id).collect();

    assert!(
        received_ids.contains(&event1_id),
        "Event 1 (before gap) must be processed"
    );
    assert!(
        received_ids.contains(&event3_id),
        "Event 3 (after gap) must be processed after gap timeout. Received: {:?}",
        received_ids
    );

    // Verify checkpoint advanced past the gap
    let checkpoint: Option<(i64,)> = sqlx::query_as(
        "SELECT last_global_sequence FROM epoch_event_bus_checkpoints WHERE subscriber_id = $1",
    )
    .bind("TestProjection")
    .fetch_optional(&pool)
    .await
    .expect("query checkpoint");

    if let Some((seq,)) = checkpoint {
        assert!(
            seq as u64 >= 2,
            "Checkpoint should have advanced past the gap, got: {}",
            seq
        );
    }

    event_bus.shutdown().await.expect("shutdown");
}

#[tokio::test]
#[serial]
async fn test_burst_concurrent_events_all_processed() {
    let Some((_pool, event_bus, event_store)) = setup().await else {
        return;
    };

    let projection = TestProjection::new();
    let projection_events = projection.get_state_store().clone();
    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe projection");

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let stream_id = Uuid::new_v4();
    let event_store = Arc::new(event_store);
    let mut handles = Vec::new();

    for i in 1..=50u64 {
        let es = event_store.clone();
        let sid = stream_id;
        handles.push(tokio::spawn(async move {
            let event = new_event(sid, i, &format!("burst_event_{}", i));
            es.store_event(event).await.expect("Failed to store event");
        }));
    }

    for h in handles {
        h.await.expect("task panicked");
    }

    // Allow time for all events to be processed (periodic tick + gap resolution)
    tokio::time::sleep(tokio::time::Duration::from_millis(3000)).await;

    let events_received = projection_events
        .get_state(stream_id)
        .await
        .unwrap()
        .expect("Should have received events");

    assert_eq!(
        events_received.0.len(),
        50,
        "All 50 burst events must be processed. Got: {}",
        events_received.0.len()
    );

    event_bus.shutdown().await.expect("shutdown");
}

#[tokio::test]
#[serial]
async fn test_multiple_subscribers_out_of_order() {
    let Some((pool, event_bus, _event_store)) = setup().await else {
        return;
    };

    let projection1 = TestProjection::new();
    let projection_events1 = projection1.get_state_store().clone();
    event_bus
        .subscribe(ProjectionHandler::new(projection1))
        .await
        .expect("Failed to subscribe projection 1");

    let projection2 = TestProjection::new();
    let projection_events2 = projection2.get_state_store().clone();
    event_bus
        .subscribe(ProjectionHandler::new(projection2))
        .await
        .expect("Failed to subscribe projection 2");

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let stream_id = Uuid::new_v4();
    let event_a_id = Uuid::new_v4();
    let event_b_id = Uuid::new_v4();

    let event_data_a = serde_json::to_value(Some(TestEventData::TestEvent {
        value: "ooo_a".to_string(),
    }))
    .unwrap();
    let event_data_b = serde_json::to_value(Some(TestEventData::TestEvent {
        value: "ooo_b".to_string(),
    }))
    .unwrap();

    // Task A: lower seq, late commit
    let pool_a = pool.clone();
    let task_a = {
        let data = event_data_a.clone();
        tokio::spawn(async move {
            let mut tx = pool_a.begin().await.expect("tx A begin");
            sqlx::query(
                r#"INSERT INTO epoch_events (id, stream_id, stream_version, event_type, data, created_at)
                 VALUES ($1, $2, $3, $4, $5, NOW())"#,
            )
            .bind(event_a_id)
            .bind(stream_id)
            .bind(1i64)
            .bind("MyEvent")
            .bind(data)
            .execute(&mut *tx)
            .await
            .expect("tx A insert");
            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
            tx.commit().await.expect("tx A commit");
        })
    };

    // Task B: higher seq, early commit
    let pool_b = pool.clone();
    let task_b = {
        let data = event_data_b.clone();
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
            let mut tx = pool_b.begin().await.expect("tx B begin");
            sqlx::query(
                r#"INSERT INTO epoch_events (id, stream_id, stream_version, event_type, data, created_at)
                 VALUES ($1, $2, $3, $4, $5, NOW())"#,
            )
            .bind(event_b_id)
            .bind(stream_id)
            .bind(2i64)
            .bind("MyEvent")
            .bind(data)
            .execute(&mut *tx)
            .await
            .expect("tx B insert");
            tx.commit().await.expect("tx B commit");
        })
    };

    task_a.await.expect("task A panicked");
    task_b.await.expect("task B panicked");

    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    for (label, store) in [
        ("projection1", &projection_events1),
        ("projection2", &projection_events2),
    ] {
        let events = store
            .get_state(stream_id)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("{} should have received events", label));
        let ids: Vec<Uuid> = events.0.iter().map(|e| e.id).collect();
        assert_eq!(
            events.0.len(),
            2,
            "{} should have 2 events, got {:?}",
            label,
            ids
        );
        assert!(ids.contains(&event_a_id), "{} missing event A", label);
        assert!(ids.contains(&event_b_id), "{} missing event B", label);
    }

    event_bus.shutdown().await.expect("shutdown");
}

#[tokio::test]
#[serial]
async fn test_catchup_plus_realtime_handoff_no_loss() {
    let Some((_pool, event_bus, event_store)) = setup().await else {
        return;
    };

    let stream_id = Uuid::new_v4();

    // Store 10 events before subscribing (catch-up path)
    for i in 1..=10 {
        let event = new_event(stream_id, i, &format!("pre_event_{}", i));
        event_store
            .store_event(event)
            .await
            .expect("Failed to store event");
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

    let projection = TestProjection::new();
    let projection_events = projection.get_state_store().clone();
    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe");

    // Wait for catch-up to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Store 10 more events (real-time path)
    for i in 11..=20 {
        let event = new_event(stream_id, i, &format!("post_event_{}", i));
        event_store
            .store_event(event)
            .await
            .expect("Failed to store event");
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    let events_received = projection_events
        .get_state(stream_id)
        .await
        .unwrap()
        .expect("Should have received events");

    assert_eq!(
        events_received.0.len(),
        20,
        "All 20 events (10 catch-up + 10 real-time) must be received. Got: {}",
        events_received.0.len()
    );

    // Verify no duplicates
    let ids: std::collections::HashSet<Uuid> = events_received.0.iter().map(|e| e.id).collect();
    assert_eq!(ids.len(), 20, "No duplicate events should be present");

    event_bus.shutdown().await.expect("shutdown");
}

#[tokio::test]
#[serial]
async fn test_batched_checkpoint_with_gap_tracking() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };

    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    common::truncate_epoch_tables(&pool).await;

    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        checkpoint_mode: epoch_pg::event_bus::CheckpointMode::Batched {
            batch_size: 3,
            max_delay_ms: 500,
        },
        gap_timeout: std::time::Duration::from_secs(1),
        ..Default::default()
    };

    let channel_name = "test_batched_gap".to_string();
    let event_bus = epoch_pg::event_bus::PgEventBus::<TestEventData>::with_config(
        pool.clone(),
        channel_name,
        config,
    );

    event_bus
        .setup_trigger()
        .await
        .expect("Failed to setup trigger");
    event_bus
        .start_listener()
        .await
        .expect("Failed to start listener");

    let projection = TestProjection::new();
    let subscriber_id = projection.subscriber_id().to_string();
    let projection_events = projection.get_state_store().clone();

    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe");

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let stream_id = Uuid::new_v4();

    // Store 5 events normally
    let event_store = PgEventStore::new(pool.clone(), event_bus.clone());
    for i in 1..=5 {
        let event = new_event(stream_id, i, &format!("batched_gap_event_{}", i));
        event_store
            .store_event(event)
            .await
            .expect("Failed to store event");
    }

    // Wait for batched flush (batch_size=3 triggers at event 3, then max_delay at 500ms for rest)
    tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;

    let checkpoint = event_bus
        .get_checkpoint(&subscriber_id)
        .await
        .expect("Failed to get checkpoint");

    assert!(
        checkpoint.is_some(),
        "Checkpoint should be written in batched mode with gap tracking"
    );

    let events_received = projection_events
        .get_state(stream_id)
        .await
        .unwrap()
        .expect("Should have received events");

    assert_eq!(
        events_received.0.len(),
        5,
        "All 5 events should be processed"
    );

    event_bus.shutdown().await.expect("shutdown");
}

#[tokio::test]
#[serial]
async fn test_undeserializable_event_advances_past() {
    let Some((pool, event_bus, _event_store)) = setup().await else {
        return;
    };

    let projection = TestProjection::new();
    let subscriber_id = projection.subscriber_id().to_string();
    let projection_events = projection.get_state_store().clone();

    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe");

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let stream_id = Uuid::new_v4();

    // Valid event 1
    let valid1_id = Uuid::new_v4();
    let valid_data = serde_json::to_value(Some(TestEventData::TestEvent {
        value: "valid_1".to_string(),
    }))
    .unwrap();
    sqlx::query(
        "INSERT INTO epoch_events (id, stream_id, stream_version, event_type, data, created_at)
         VALUES ($1, $2, 1, 'MyEvent', $3, NOW())",
    )
    .bind(valid1_id)
    .bind(stream_id)
    .bind(&valid_data)
    .execute(&pool)
    .await
    .expect("insert valid 1");

    // Malformed event
    let malformed_id = Uuid::new_v4();
    let malformed_data = serde_json::json!({"garbage": true});
    sqlx::query(
        "INSERT INTO epoch_events (id, stream_id, stream_version, event_type, data, created_at)
         VALUES ($1, $2, 2, 'MyEvent', $3, NOW())",
    )
    .bind(malformed_id)
    .bind(stream_id)
    .bind(&malformed_data)
    .execute(&pool)
    .await
    .expect("insert malformed");

    // Valid event 3
    let valid3_id = Uuid::new_v4();
    let valid3_data = serde_json::to_value(Some(TestEventData::TestEvent {
        value: "valid_3".to_string(),
    }))
    .unwrap();
    sqlx::query(
        "INSERT INTO epoch_events (id, stream_id, stream_version, event_type, data, created_at)
         VALUES ($1, $2, 3, 'MyEvent', $3, NOW())",
    )
    .bind(valid3_id)
    .bind(stream_id)
    .bind(&valid3_data)
    .execute(&pool)
    .await
    .expect("insert valid 3");

    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    let events_received = projection_events
        .get_state(stream_id)
        .await
        .unwrap()
        .expect("Should have received events");

    assert_eq!(
        events_received.0.len(),
        2,
        "Only valid events should be received"
    );
    let ids: Vec<Uuid> = events_received.0.iter().map(|e| e.id).collect();
    assert!(ids.contains(&valid1_id), "Valid event 1 should be received");
    assert!(ids.contains(&valid3_id), "Valid event 3 should be received");
    assert!(
        !ids.contains(&malformed_id),
        "Malformed event should NOT be received"
    );

    // Verify checkpoint advanced past the malformed event
    let checkpoint = event_bus
        .get_checkpoint(&subscriber_id)
        .await
        .expect("Failed to get checkpoint");
    assert!(
        checkpoint.is_some(),
        "Checkpoint should have advanced past malformed event"
    );

    // Clean up malformed event
    sqlx::query("DELETE FROM epoch_events WHERE id = $1")
        .bind(malformed_id)
        .execute(&pool)
        .await
        .expect("cleanup");

    event_bus.shutdown().await.expect("shutdown");
}

#[tokio::test]
#[serial]
async fn test_graceful_shutdown_flushes_subscriber_states() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };

    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    common::truncate_epoch_tables(&pool).await;

    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        checkpoint_mode: epoch_pg::event_bus::CheckpointMode::Batched {
            batch_size: 1000,    // Won't auto-flush
            max_delay_ms: 60000, // Won't trigger
        },
        ..Default::default()
    };

    let channel_name = "test_shutdown_states".to_string();
    let event_bus = PgEventBus::<TestEventData>::with_config(pool.clone(), channel_name, config);
    let event_store = PgEventStore::new(pool.clone(), event_bus.clone());

    event_bus
        .setup_trigger()
        .await
        .expect("Failed to setup trigger");
    event_bus
        .start_listener()
        .await
        .expect("Failed to start listener");

    let projection = TestProjection::new();
    let subscriber_id = projection.subscriber_id().to_string();

    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe");

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Store 3 events
    let stream_id = Uuid::new_v4();
    for i in 1..=3 {
        let event = new_event(stream_id, i, &format!("shutdown_event_{}", i));
        event_store
            .store_event(event)
            .await
            .expect("Failed to store event");
    }

    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Shutdown should flush
    event_bus.shutdown().await.expect("Failed to shutdown");

    let checkpoint = event_bus
        .get_checkpoint(&subscriber_id)
        .await
        .expect("Failed to get checkpoint");

    assert!(
        checkpoint.is_some(),
        "Checkpoint should be flushed on graceful shutdown with subscriber states"
    );
}

// ============================================================================
// Gap-timeout observability integration tests (spec 0016 / CLOUD-169)
//
// These tests exercise the durable gap-timeout machinery end-to-end: when a
// subscriber's checkpoint advances past a permanent hole in `global_sequence`
// after `gap_timeout`, a record is written to `epoch_event_bus_gap_timeouts`,
// the `on_gap_timeout` callback fires, and operators can list/resolve records.
//
// All tests are `#[serial]`, use a short `gap_timeout`, fresh `Uuid::new_v4()`
// stream IDs, and scope every assertion to their own randomly-generated
// subscriber ID so that they remain idempotent across repeated runs and never
// observe records produced by sibling tests (NFR-5).
// ============================================================================

use epoch_pg::event_bus::{GapTimeoutCallback, GapTimeoutEntry, GapTimeoutInfo};
use std::time::Duration as GapDuration;

/// Inserts a committed event for `stream_id` into `table` and returns its
/// assigned `global_sequence`.
///
/// `table` is the events table to write to. Tests running on an isolated events
/// table (see [`isolated_events_table`]) MUST pass that table's name so the
/// event lands on the private sequence rather than the shared `epoch_events`
/// one; passing `"epoch_events"` reproduces the original shared-table behaviour.
async fn insert_committed_event(
    pool: &PgPool,
    table: &str,
    stream_id: Uuid,
    version: i64,
    value: &str,
) -> (Uuid, i64) {
    let id = Uuid::new_v4();
    let data = serde_json::to_value(Some(TestEventData::TestEvent {
        value: value.to_string(),
    }))
    .unwrap();
    let seq: i64 = sqlx::query_scalar(&format!(
        r#"INSERT INTO {table} (id, stream_id, stream_version, event_type, data, created_at)
           VALUES ($1, $2, $3, 'MyEvent', $4, NOW())
           RETURNING global_sequence"#
    ))
    .bind(id)
    .bind(stream_id)
    .bind(version)
    .bind(&data)
    .fetch_one(pool)
    .await
    .expect("insert committed event");
    (id, seq)
}

/// Creates a deterministic, permanent hole in `global_sequence` for `stream_id`.
///
/// Commits an event before the gap (seq N), consumes the next sequence value
/// (N+1) inside a transaction that is rolled back (so N+1 never commits — the
/// permanent gap), then commits an event after the gap (N+2). Because the
/// integration suite runs `#[serial]`, no other writer interleaves, so the
/// rolled-back transaction is guaranteed to own the missing sequence.
///
/// Returns `(before_event_id, skipped_sequence, after_event_id)`.
///
/// `table` is the events table the gap is burned on. On an isolated table the
/// hole is claimed via *that* table's `DEFAULT nextval`, so it burns the private
/// sequence; passing `"epoch_events"` reproduces the original shared-table
/// behaviour.
async fn create_sequence_gap(pool: &PgPool, table: &str, stream_id: Uuid) -> (Uuid, u64, Uuid) {
    let (before_id, _before_seq) =
        insert_committed_event(pool, table, stream_id, 1, "gap_before").await;

    // A rolled-back transaction consumes the next sequence value, which will
    // never commit — producing a permanent hole the subscriber must skip.
    let skipped_seq: i64 = {
        let mut tx = pool.begin().await.expect("begin rollback tx");
        let data = serde_json::to_value(Some(TestEventData::TestEvent {
            value: "rolled_back".to_string(),
        }))
        .unwrap();
        let seq: i64 = sqlx::query_scalar(&format!(
            r#"INSERT INTO {table} (id, stream_id, stream_version, event_type, data, created_at)
               VALUES ($1, $2, 2, 'MyEvent', $3, NOW())
               RETURNING global_sequence"#
        ))
        .bind(Uuid::new_v4())
        .bind(stream_id)
        .bind(&data)
        .fetch_one(&mut *tx)
        .await
        .expect("insert rolled-back event");
        tx.rollback().await.expect("rollback gap tx");
        seq
    };

    let (after_id, _after_seq) =
        insert_committed_event(pool, table, stream_id, 3, "gap_after").await;

    (before_id, skipped_seq as u64, after_id)
}

/// Builds and starts a `PgEventBus` with the given config, subscribes a fresh
/// `TestProjection`, and returns the bus, the subscriber's ID, and its state
/// store for assertions.
async fn start_gap_test_bus(
    pool: &PgPool,
    mut config: epoch_pg::event_bus::ReliableDeliveryConfig,
) -> (
    PgEventBus<TestEventData>,
    String,
    InMemoryStateStore<TestState>,
) {
    // These CLOUD-169 observability tests construct their gaps with *committed*
    // rolled-back transactions, so under the CLOUD-180 default (`snapshot_fencing
    // = true`) the resolver would prove the gap permanent and skip it as
    // `FenceCleared` — without recording a gap-timeout row. To keep exercising
    // the backstop recording machinery they intentionally target the legacy
    // timeout-only path. The fence-aware behaviour is covered separately by the
    // dedicated CLOUD-180 integration tests.
    config.snapshot_fencing = false;
    let channel_name = "test_gap_obs".to_string();
    let event_bus = PgEventBus::<TestEventData>::with_config(pool.clone(), channel_name, config);
    event_bus
        .setup_trigger()
        .await
        .expect("Failed to setup event bus trigger");
    event_bus
        .start_listener()
        .await
        .expect("Failed to start event bus listener");

    let projection = TestProjection::new();
    let subscriber_id = projection.subscriber_id().to_string();
    let projection_events = projection.get_state_store().clone();
    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe projection");

    // Let catch-up and buffer setup complete before producing the gap.
    tokio::time::sleep(GapDuration::from_millis(100)).await;

    (event_bus, subscriber_id, projection_events)
}

/// Polls `list_gap_timeouts` until a record for `skipped_sequence` appears for
/// `subscriber_id`, or the bounded retry window elapses.
async fn poll_for_gap_record(
    event_bus: &PgEventBus<TestEventData>,
    subscriber_id: &str,
    skipped_sequence: u64,
) -> Option<GapTimeoutEntry> {
    for _ in 0..24 {
        let entries = event_bus
            .list_gap_timeouts(Some(subscriber_id), false, 0, 50)
            .await
            .expect("Failed to list gap timeouts");
        if let Some(entry) = entries
            .into_iter()
            .find(|e| e.skipped_sequence == skipped_sequence)
        {
            return Some(entry);
        }
        tokio::time::sleep(GapDuration::from_millis(250)).await;
    }
    None
}

#[tokio::test]
#[serial]
async fn test_gap_timeout_inserts_record() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    common::truncate_epoch_tables(&pool).await;

    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        gap_timeout: GapDuration::from_millis(500),
        ..Default::default()
    };
    let (event_bus, subscriber_id, projection_events) = start_gap_test_bus(&pool, config).await;

    let stream_id = Uuid::new_v4();
    let (before_id, skipped_seq, after_id) =
        create_sequence_gap(&pool, "epoch_events", stream_id).await;

    let entry = poll_for_gap_record(&event_bus, &subscriber_id, skipped_seq)
        .await
        .expect("a gap-timeout record should be inserted for the skipped sequence");

    // The record captures bus, subscriber, sequence, and an elapsed duration that
    // is at least the configured gap_timeout (AC-2).
    assert_eq!(entry.subscriber_id, subscriber_id);
    assert_eq!(entry.bus_name, "epoch_events");
    assert_eq!(entry.skipped_sequence, skipped_seq);
    assert!(
        entry.resolved_at.is_none(),
        "record should start unresolved"
    );
    assert!(
        entry.gap_duration_ms >= 500,
        "gap_duration_ms ({}) should be >= gap_timeout (500ms)",
        entry.gap_duration_ms
    );

    // The checkpoint advanced past the gap, so both committed events were
    // delivered despite the hole (NFR-1: recording never gates advancement).
    let state = projection_events
        .get_state(stream_id)
        .await
        .unwrap()
        .expect("Should have received events around the gap");
    let received_ids: Vec<Uuid> = state.0.iter().map(|e| e.id).collect();
    assert!(
        received_ids.contains(&before_id),
        "event before the gap must be delivered"
    );
    assert!(
        received_ids.contains(&after_id),
        "event after the gap must be delivered once the gap times out"
    );

    let checkpoint = event_bus
        .get_checkpoint(&subscriber_id)
        .await
        .expect("Failed to get checkpoint");
    assert!(
        matches!(checkpoint, Some(seq) if seq >= skipped_seq),
        "checkpoint ({checkpoint:?}) should have advanced past the skipped sequence {skipped_seq}"
    );

    event_bus.shutdown().await.expect("shutdown");
}

#[tokio::test]
#[serial]
async fn test_event_committed_after_gap_timeout_is_reported_as_skipped() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    common::truncate_epoch_tables(&pool).await;

    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        gap_timeout: GapDuration::from_millis(500),
        ..Default::default()
    };
    let (event_bus, subscriber_id, projection_events) = start_gap_test_bus(&pool, config).await;

    let stream_id = Uuid::new_v4();
    let (_before_id, skipped_seq, _after_id) =
        create_sequence_gap(&pool, "epoch_events", stream_id).await;

    // Wait for the gap to time out and be recorded.
    let _entry = poll_for_gap_record(&event_bus, &subscriber_id, skipped_seq)
        .await
        .expect("gap-timeout record should exist before the late commit");

    // Now the "late" event commits at the previously-skipped global_sequence —
    // simulating a transaction that was actually still in flight. Because the
    // checkpoint already advanced past it, it must NOT be delivered.
    let late_id = Uuid::new_v4();
    let late_data = serde_json::to_value(Some(TestEventData::TestEvent {
        value: "late_commit".to_string(),
    }))
    .unwrap();
    sqlx::query(
        r#"INSERT INTO epoch_events
               (id, stream_id, stream_version, event_type, data, created_at, global_sequence)
           VALUES ($1, $2, 2, 'MyEvent', $3, NOW(), $4)"#,
    )
    .bind(late_id)
    .bind(stream_id)
    .bind(&late_data)
    .bind(skipped_seq as i64)
    .execute(&pool)
    .await
    .expect("insert late-committed event at the skipped sequence");

    // Give the listener ample time to (not) deliver the late event.
    tokio::time::sleep(GapDuration::from_millis(1500)).await;

    let state = projection_events
        .get_state(stream_id)
        .await
        .unwrap()
        .expect("Should have received the surrounding events");
    let received_ids: Vec<Uuid> = state.0.iter().map(|e| e.id).collect();
    assert!(
        !received_ids.contains(&late_id),
        "the late-committed event at the skipped sequence must NOT be delivered to this subscriber"
    );

    // ...but the skip is visibly reported: the durable record persists (AC-4).
    let entries = event_bus
        .list_gap_timeouts(Some(&subscriber_id), false, 0, 50)
        .await
        .expect("Failed to list gap timeouts");
    assert!(
        entries.iter().any(|e| e.skipped_sequence == skipped_seq),
        "the gap-timeout record for the skipped sequence must remain queryable"
    );

    // Clean up the late event so it does not pollute later tests.
    sqlx::query("DELETE FROM epoch_events WHERE id = $1")
        .bind(late_id)
        .execute(&pool)
        .await
        .expect("cleanup late event");

    event_bus.shutdown().await.expect("shutdown");
}

#[tokio::test]
#[serial]
async fn test_gap_timeout_callback_is_invoked() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    common::truncate_epoch_tables(&pool).await;

    #[derive(Default)]
    struct RecordingGapCallback {
        infos: std::sync::Mutex<Vec<GapTimeoutInfo>>,
    }

    #[async_trait::async_trait]
    impl GapTimeoutCallback for RecordingGapCallback {
        async fn on_gap_timeout(&self, info: GapTimeoutInfo) {
            self.infos.lock().unwrap().push(info);
        }
    }

    let callback = Arc::new(RecordingGapCallback::default());
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        gap_timeout: GapDuration::from_millis(500),
        on_gap_timeout: Some(callback.clone()),
        ..Default::default()
    };
    let (event_bus, subscriber_id, _projection_events) = start_gap_test_bus(&pool, config).await;

    let stream_id = Uuid::new_v4();
    let (_before_id, skipped_seq, _after_id) =
        create_sequence_gap(&pool, "epoch_events", stream_id).await;

    // Wait for the durable record (the callback fires after persistence).
    poll_for_gap_record(&event_bus, &subscriber_id, skipped_seq)
        .await
        .expect("gap-timeout record should exist");

    // Poll for the callback to fire for this subscriber's skipped sequence.
    let mut matching: Vec<GapTimeoutInfo> = Vec::new();
    for _ in 0..24 {
        matching = callback
            .infos
            .lock()
            .unwrap()
            .iter()
            .filter(|i| i.subscriber_id == subscriber_id && i.skipped_sequence == skipped_seq)
            .cloned()
            .collect();
        if !matching.is_empty() {
            break;
        }
        tokio::time::sleep(GapDuration::from_millis(250)).await;
    }

    assert_eq!(
        matching.len(),
        1,
        "on_gap_timeout must fire exactly once for the skipped sequence (got {})",
        matching.len()
    );
    let info = &matching[0];
    assert_eq!(info.bus_name, "epoch_events");
    assert_eq!(info.subscriber_id, subscriber_id);
    assert_eq!(info.skipped_sequence, skipped_seq);
    assert!(
        info.gap_duration >= GapDuration::from_millis(500),
        "callback gap_duration ({:?}) should be >= gap_timeout (500ms)",
        info.gap_duration
    );

    event_bus.shutdown().await.expect("shutdown");
}

#[tokio::test]
#[serial]
async fn test_list_gap_timeouts_returns_entries() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    common::truncate_epoch_tables(&pool).await;

    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        gap_timeout: GapDuration::from_millis(500),
        ..Default::default()
    };
    let (event_bus, subscriber_id, _projection_events) = start_gap_test_bus(&pool, config).await;

    let stream_id = Uuid::new_v4();
    let (_before_id, skipped_seq, _after_id) =
        create_sequence_gap(&pool, "epoch_events", stream_id).await;

    poll_for_gap_record(&event_bus, &subscriber_id, skipped_seq)
        .await
        .expect("gap-timeout record should exist");

    // Listing scoped to this subscriber returns the record (AC-2).
    let all = event_bus
        .list_gap_timeouts(Some(&subscriber_id), false, 0, 50)
        .await
        .expect("Failed to list gap timeouts");
    assert!(
        all.iter().any(|e| e.skipped_sequence == skipped_seq),
        "list_gap_timeouts should return the recorded skip"
    );

    // The unresolved filter also returns it while it is unresolved.
    let unresolved = event_bus
        .list_gap_timeouts(Some(&subscriber_id), true, 0, 50)
        .await
        .expect("Failed to list unresolved gap timeouts");
    assert!(
        unresolved.iter().any(|e| e.skipped_sequence == skipped_seq),
        "unresolved listing should include the unresolved record"
    );

    event_bus.shutdown().await.expect("shutdown");
}

#[tokio::test]
#[serial]
async fn test_resolve_gap_timeout_marks_resolved() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    common::truncate_epoch_tables(&pool).await;

    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        gap_timeout: GapDuration::from_millis(500),
        ..Default::default()
    };
    let (event_bus, subscriber_id, _projection_events) = start_gap_test_bus(&pool, config).await;

    let stream_id = Uuid::new_v4();
    let (_before_id, skipped_seq, _after_id) =
        create_sequence_gap(&pool, "epoch_events", stream_id).await;

    let entry = poll_for_gap_record(&event_bus, &subscriber_id, skipped_seq)
        .await
        .expect("gap-timeout record should exist");

    // Resolving an unresolved record succeeds (FR-9).
    let resolved = event_bus
        .resolve_gap_timeout(entry.id, "operator", Some("rolled back"))
        .await
        .expect("Failed to resolve gap timeout");
    assert!(
        resolved,
        "resolving an unresolved record should return true"
    );

    // It drops out of the unresolved listing...
    let unresolved = event_bus
        .list_gap_timeouts(Some(&subscriber_id), true, 0, 50)
        .await
        .expect("Failed to list unresolved gap timeouts");
    assert!(
        !unresolved.iter().any(|e| e.id == entry.id),
        "a resolved record should not appear in the unresolved listing"
    );

    // ...but is still present overall, now carrying resolution metadata.
    let all = event_bus
        .list_gap_timeouts(Some(&subscriber_id), false, 0, 50)
        .await
        .expect("Failed to list gap timeouts");
    let resolved_entry = all
        .iter()
        .find(|e| e.id == entry.id)
        .expect("resolved record should still be queryable");
    assert!(resolved_entry.resolved_at.is_some());
    assert_eq!(resolved_entry.resolved_by.as_deref(), Some("operator"));
    assert_eq!(
        resolved_entry.resolution_notes.as_deref(),
        Some("rolled back")
    );

    // Resolving the same record again is a no-op.
    let second = event_bus
        .resolve_gap_timeout(entry.id, "operator", Some("again"))
        .await
        .expect("Failed to resolve gap timeout again");
    assert!(
        !second,
        "resolving an already-resolved record should return false"
    );

    event_bus.shutdown().await.expect("shutdown");
}

#[tokio::test]
#[serial]
async fn test_no_gap_timeout_record_on_in_order_events() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    common::truncate_epoch_tables(&pool).await;

    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        gap_timeout: GapDuration::from_millis(500),
        ..Default::default()
    };
    let (event_bus, subscriber_id, projection_events) = start_gap_test_bus(&pool, config).await;
    let event_store = PgEventStore::new(pool.clone(), event_bus.clone());

    let stream_id = Uuid::new_v4();
    for i in 1..=5u64 {
        event_store
            .store_event(new_event(stream_id, i, &format!("in_order_{i}")))
            .await
            .expect("Failed to store event");
    }

    // Wait well beyond gap_timeout so any (spurious) gap would have fired.
    tokio::time::sleep(GapDuration::from_millis(2000)).await;

    let state = projection_events
        .get_state(stream_id)
        .await
        .unwrap()
        .expect("Should have received events");
    assert_eq!(state.0.len(), 5, "all in-order events should be delivered");

    let entries = event_bus
        .list_gap_timeouts(Some(&subscriber_id), false, 0, 50)
        .await
        .expect("Failed to list gap timeouts");
    assert!(
        entries.is_empty(),
        "in-order delivery must not produce any gap-timeout records, got {entries:?}"
    );

    event_bus.shutdown().await.expect("shutdown");
}

#[tokio::test]
#[serial]
async fn test_gap_timeout_record_insert_is_idempotent() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    common::truncate_epoch_tables(&pool).await;

    // Re-recording the same (bus_name, subscriber_id, skipped_sequence) is a
    // no-op thanks to the UNIQUE constraint + ON CONFLICT DO NOTHING (NFR-6).
    let subscriber_id = format!("projection:idem:{}", Uuid::new_v4());
    let bus_name = "epoch_events";
    let skipped_seq = 1i64;

    for _ in 0..2 {
        sqlx::query(
            r#"INSERT INTO epoch_event_bus_gap_timeouts
                   (bus_name, subscriber_id, skipped_sequence, gap_duration_ms)
               VALUES ($1, $2, $3, $4)
               ON CONFLICT (bus_name, subscriber_id, skipped_sequence) DO NOTHING"#,
        )
        .bind(bus_name)
        .bind(&subscriber_id)
        .bind(skipped_seq)
        .bind(500i64)
        .execute(&pool)
        .await
        .expect("insert gap-timeout record");
    }

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM epoch_event_bus_gap_timeouts WHERE subscriber_id = $1",
    )
    .bind(&subscriber_id)
    .fetch_one(&pool)
    .await
    .expect("count gap-timeout records");
    assert_eq!(
        count, 1,
        "duplicate inserts for the same key must collapse to a single row"
    );

    // Clean up this test's row.
    sqlx::query("DELETE FROM epoch_event_bus_gap_timeouts WHERE subscriber_id = $1")
        .bind(&subscriber_id)
        .execute(&pool)
        .await
        .expect("cleanup idempotency row");
}

// ============================================================================
// Buffer-drain tests (spec 0018 / CLOUD-155)
//
// These tests exercise the post-catch-up buffer drain that runs inside
// `subscribe()`.  The drain reads events that arrived via NOTIFY *during*
// the catch-up phase and processes them before the subscriber joins the live
// listener.  Two properties specific to the refactored drain are verified:
//
//   1. Pagination: when more notifications arrive during catch-up than fit in
//      a single `catch_up_batch_size` query, the loop iterates until all
//      events have been fetched and processed.
//
//   2. Unified checkpoint tracking: a deserialize error in the drain does not
//      block subsequent events – the checkpoint advances past the malformed
//      row and the following valid events are delivered.
// ============================================================================

/// Verifies that the buffer drain correctly paginates when the number of
/// events buffered during catch-up exceeds `catch_up_batch_size`.
///
/// Setup:
/// - 30 pre-existing events drive the catch-up loop through ~10 DB
///   round-trips at `catch_up_batch_size = 3`, giving the concurrent task
///   time to insert events that land in the buffer.
/// - A spawned task inserts 9 events (3 × batch_size) while catch-up runs;
///   their NOTIFYs are captured by the buffer listener and the drain must
///   issue at least 3 pages to process them all.
/// - Even if some "during" events arrive after `subscribe()` returns (and go
///   through the live listener instead), all 39 events must be received
///   exactly once — so the assertion is correct regardless of timing.
#[tokio::test]
#[serial]
async fn test_buffer_drain_pagination_processes_all_events() {
    let Some((pool, _setup_bus, _)) = setup().await else {
        return;
    };

    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        catch_up_batch_size: 3,
        ..Default::default()
    };
    let channel_name = "test_drain_page".to_string();
    let event_bus = PgEventBus::<TestEventData>::with_config(pool.clone(), channel_name, config);
    event_bus.setup_trigger().await.expect("setup trigger");
    event_bus.start_listener().await.expect("start listener");

    // Insert 30 pre-existing events so catch-up takes ~10 DB round-trips.
    let stream_id = Uuid::new_v4();
    for i in 1i64..=30 {
        let data = serde_json::to_value(Some(TestEventData::TestEvent {
            value: format!("pre_{}", i),
        }))
        .unwrap();
        sqlx::query(
            "INSERT INTO epoch_events \
             (id, stream_id, stream_version, event_type, data, created_at) \
             VALUES ($1, $2, $3, 'MyEvent', $4, NOW())",
        )
        .bind(Uuid::new_v4())
        .bind(stream_id)
        .bind(i)
        .bind(&data)
        .execute(&pool)
        .await
        .expect("insert pre event");
    }

    // Spawn a task that inserts 9 events (> batch_size) concurrently with
    // the catch-up loop.  The buffer listener captures their NOTIFYs, so the
    // drain must iterate multiple pages to process them all.
    let pool2 = pool.clone();
    let during_task = tokio::spawn(async move {
        // Tiny delay so the buffer listener has connected before we insert.
        tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
        for i in 31i64..=39 {
            let data = serde_json::to_value(Some(TestEventData::TestEvent {
                value: format!("during_{}", i),
            }))
            .unwrap();
            sqlx::query(
                "INSERT INTO epoch_events \
                 (id, stream_id, stream_version, event_type, data, created_at) \
                 VALUES ($1, $2, $3, 'MyEvent', $4, NOW())",
            )
            .bind(Uuid::new_v4())
            .bind(stream_id)
            .bind(i)
            .bind(&data)
            .execute(&pool2)
            .await
            .expect("insert during event");
        }
    });

    let projection = TestProjection::new();
    let projection_events = projection.get_state_store().clone();

    // subscribe() runs the catch-up loop while the spawned task is running.
    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("subscribe");

    during_task.await.expect("during task completed");

    // Allow the live listener to deliver any events that arrived after
    // subscribe() returned rather than going through the buffer drain.
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    let state = projection_events
        .get_state(stream_id)
        .await
        .unwrap()
        .expect("state should exist");

    assert_eq!(
        state.0.len(),
        39,
        "all 39 events (30 pre-existing + 9 during catch-up) must be received exactly once"
    );

    event_bus.shutdown().await.expect("shutdown");
}

/// Verifies that a deserialisation failure in the post-catch-up buffer drain:
///
/// - does not prevent subsequent valid events from being delivered, and
/// - advances the checkpoint past the malformed event so the subscriber
///   does not get stuck in an infinite retry loop.
///
/// This exercises the unified checkpoint-tracking path introduced in the
/// refactor: previously the error branch duplicated the checkpoint-flush code
/// via `continue`; now it falls through to a single block at the bottom of
/// the loop iteration.
#[tokio::test]
#[serial]
async fn test_buffer_drain_deser_error_advances_checkpoint() {
    let Some((pool, _setup_bus, _)) = setup().await else {
        return;
    };

    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        catch_up_batch_size: 2,
        ..Default::default()
    };
    let channel_name = "test_drain_deser".to_string();
    let event_bus = PgEventBus::<TestEventData>::with_config(pool.clone(), channel_name, config);
    event_bus.setup_trigger().await.expect("setup trigger");
    event_bus.start_listener().await.expect("start listener");

    // 10 pre-existing events → 5 catch-up iterations at batch_size=2,
    // giving the concurrent task time to insert buffered events.
    let stream_id = Uuid::new_v4();
    for i in 1i64..=10 {
        let data = serde_json::to_value(Some(TestEventData::TestEvent {
            value: format!("pre_{}", i),
        }))
        .unwrap();
        sqlx::query(
            "INSERT INTO epoch_events \
             (id, stream_id, stream_version, event_type, data, created_at) \
             VALUES ($1, $2, $3, 'MyEvent', $4, NOW())",
        )
        .bind(Uuid::new_v4())
        .bind(stream_id)
        .bind(i)
        .bind(&data)
        .execute(&pool)
        .await
        .expect("insert pre event");
    }

    let valid1_id = Uuid::new_v4();
    let malformed_id = Uuid::new_v4();
    let valid2_id = Uuid::new_v4();

    // Insert valid1, malformed, valid2 while catch-up runs so they land in
    // the buffer (or, if timing places them after catch-up, in the live
    // listener — both paths must handle deserialization errors identically).
    let pool2 = pool.clone();
    let during_task = tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;

        let valid1_data = serde_json::to_value(Some(TestEventData::TestEvent {
            value: "valid_during_1".to_string(),
        }))
        .unwrap();
        sqlx::query(
            "INSERT INTO epoch_events \
             (id, stream_id, stream_version, event_type, data, created_at) \
             VALUES ($1, $2, 11, 'MyEvent', $3, NOW())",
        )
        .bind(valid1_id)
        .bind(stream_id)
        .bind(&valid1_data)
        .execute(&pool2)
        .await
        .expect("insert valid1");

        // JSON object that cannot be deserialized as TestEventData.
        let malformed_data = serde_json::json!({"garbage": true});
        sqlx::query(
            "INSERT INTO epoch_events \
             (id, stream_id, stream_version, event_type, data, created_at) \
             VALUES ($1, $2, 12, 'MyEvent', $3, NOW())",
        )
        .bind(malformed_id)
        .bind(stream_id)
        .bind(&malformed_data)
        .execute(&pool2)
        .await
        .expect("insert malformed");

        let valid2_data = serde_json::to_value(Some(TestEventData::TestEvent {
            value: "valid_during_2".to_string(),
        }))
        .unwrap();
        sqlx::query(
            "INSERT INTO epoch_events \
             (id, stream_id, stream_version, event_type, data, created_at) \
             VALUES ($1, $2, 13, 'MyEvent', $3, NOW())",
        )
        .bind(valid2_id)
        .bind(stream_id)
        .bind(&valid2_data)
        .execute(&pool2)
        .await
        .expect("insert valid2");
    });

    let projection = TestProjection::new();
    let subscriber_id = projection.subscriber_id().to_string();
    let projection_events = projection.get_state_store().clone();

    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("subscribe");

    during_task.await.expect("during task completed");

    // Wait for the live listener to deliver events that arrived after
    // subscribe() returned.
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    let state = projection_events
        .get_state(stream_id)
        .await
        .unwrap()
        .expect("state should exist");

    // 10 pre-existing + valid1 + valid2 = 12; malformed must be skipped.
    assert_eq!(
        state.0.len(),
        12,
        "malformed event must be skipped; valid events before and after must be delivered"
    );

    let ids: Vec<Uuid> = state.0.iter().map(|e| e.id).collect();
    assert!(
        ids.contains(&valid1_id),
        "valid event before malformed must be received"
    );
    assert!(
        ids.contains(&valid2_id),
        "valid event after malformed must be received"
    );
    assert!(
        !ids.contains(&malformed_id),
        "malformed event must NOT be received"
    );

    // Checkpoint must have advanced past the malformed event's sequence so
    // the subscriber does not loop forever trying to re-process it.
    let checkpoint = event_bus
        .get_checkpoint(&subscriber_id)
        .await
        .expect("get checkpoint");
    assert!(
        checkpoint.is_some(),
        "checkpoint must exist and have advanced past the malformed event"
    );

    // Remove the malformed row so subsequent tests that scan all events
    // don't trip over it.
    sqlx::query("DELETE FROM epoch_events WHERE id = $1")
        .bind(malformed_id)
        .execute(&pool)
        .await
        .expect("cleanup malformed event");

    event_bus.shutdown().await.expect("shutdown");
}

// ============================================================================
// Snapshot-fencing integration tests (spec 0019 / CLOUD-180)
//
// These tests exercise the snapshot-fencing gap resolver end-to-end against a
// live PostgreSQL (PG13+). They orchestrate real concurrent transactions — an
// in-flight writer, a rolled-back writer, and an unrelated `xmin`-pinning
// sentinel — so the `txid`/snapshot interaction is exercised for real rather
// than simulated. All tests are `#[serial]`, use fresh `Uuid::new_v4()` streams,
// and scope assertions to their own randomly-generated subscriber id.
// ============================================================================

/// Builds and starts a `PgEventBus` honouring the supplied `config` (in
/// particular, **not** forcing `snapshot_fencing`), subscribes a fresh
/// `TestProjection`, and returns the bus, its subscriber id, and state store.
async fn start_fence_test_bus(
    pool: &PgPool,
    config: epoch_pg::event_bus::ReliableDeliveryConfig,
) -> (
    PgEventBus<TestEventData>,
    String,
    InMemoryStateStore<TestState>,
) {
    let channel_name = "test_fence".to_string();
    let event_bus = PgEventBus::<TestEventData>::with_config(pool.clone(), channel_name, config);
    event_bus
        .setup_trigger()
        .await
        .expect("Failed to setup event bus trigger");
    event_bus
        .start_listener()
        .await
        .expect("Failed to start event bus listener");

    let projection = TestProjection::new();
    let subscriber_id = projection.subscriber_id().to_string();
    let projection_events = projection.get_state_store().clone();
    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe projection");

    // Let catch-up and buffer setup complete before producing the gap.
    tokio::time::sleep(GapDuration::from_millis(100)).await;

    (event_bus, subscriber_id, projection_events)
}

/// FR-1 / FR-2: after migrations `epoch_events` has a `txid BIGINT` column that
/// new inserts populate via the `pg_current_xact_id()` DEFAULT, while rows
/// written with an explicit `NULL` (simulating pre-m011 history) are tolerated.
#[tokio::test]
#[serial]
async fn test_txid_column_exists_and_populates() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    common::truncate_epoch_tables(&pool).await;

    // The column exists after m011 and is a BIGINT.
    let col: Option<(String,)> = sqlx::query_as(
        "SELECT data_type FROM information_schema.columns \
         WHERE table_name = 'epoch_events' AND column_name = 'txid'",
    )
    .fetch_optional(&pool)
    .await
    .expect("query txid column");
    let (data_type,) = col.expect("txid column must exist on epoch_events after m011");
    assert_eq!(data_type, "bigint", "txid must be a BIGINT column");

    // A new insert populates txid via the DEFAULT.
    let stream_id = Uuid::new_v4();
    let id = Uuid::new_v4();
    let data = serde_json::to_value(Some(TestEventData::TestEvent {
        value: "txid_populates".to_string(),
    }))
    .unwrap();
    sqlx::query(
        r#"INSERT INTO epoch_events (id, stream_id, stream_version, event_type, data, created_at)
           VALUES ($1, $2, 1, 'MyEvent', $3, NOW())"#,
    )
    .bind(id)
    .bind(stream_id)
    .bind(&data)
    .execute(&pool)
    .await
    .expect("insert event");
    let txid: Option<i64> = sqlx::query_scalar("SELECT txid FROM epoch_events WHERE id = $1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("query txid");
    assert!(
        txid.is_some(),
        "a new insert must populate txid via the column DEFAULT"
    );

    // A row written with an explicit NULL txid (pre-m011 history) is tolerated.
    let legacy_id = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO epoch_events
               (id, stream_id, stream_version, event_type, data, created_at, txid)
           VALUES ($1, $2, 2, 'MyEvent', $3, NOW(), NULL)"#,
    )
    .bind(legacy_id)
    .bind(stream_id)
    .bind(&data)
    .execute(&pool)
    .await
    .expect("insert legacy NULL-txid row");
    let legacy_txid: Option<i64> =
        sqlx::query_scalar("SELECT txid FROM epoch_events WHERE id = $1")
            .bind(legacy_id)
            .fetch_one(&pool)
            .await
            .expect("query legacy txid");
    assert!(
        legacy_txid.is_none(),
        "an explicit NULL txid must be tolerated (pre-migration rows keep NULL)"
    );

    // Clean up this test's rows.
    sqlx::query("DELETE FROM epoch_events WHERE id = ANY($1)")
        .bind(vec![id, legacy_id])
        .execute(&pool)
        .await
        .expect("cleanup txid test rows");
}

/// Issue test (a) / FR-5: a gap whose writer is still in-flight must be HELD —
/// the checkpoint does not advance and no gap-timeout row is recorded — until
/// the writer commits, after which the previously-missing event is delivered and
/// the checkpoint advances.
#[tokio::test]
#[serial]
async fn test_in_flight_transaction_gap_is_held() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    common::truncate_epoch_tables(&pool).await;

    // Generous gap_timeout so the backstop cannot fire while we observe the hold.
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        gap_timeout: GapDuration::from_secs(30),
        ..Default::default()
    };
    let (event_bus, subscriber_id, projection_events) = start_fence_test_bus(&pool, config).await;

    let stream_id = Uuid::new_v4();

    // Open transaction A and claim the next global_sequence (N) WITHOUT committing.
    let mut tx_a = pool.begin().await.expect("begin in-flight tx A");
    let in_flight_id = Uuid::new_v4();
    let data = serde_json::to_value(Some(TestEventData::TestEvent {
        value: "in_flight_N".to_string(),
    }))
    .unwrap();
    let seq_n: i64 = sqlx::query_scalar(
        r#"INSERT INTO epoch_events (id, stream_id, stream_version, event_type, data, created_at)
           VALUES ($1, $2, 1, 'MyEvent', $3, NOW())
           RETURNING global_sequence"#,
    )
    .bind(in_flight_id)
    .bind(stream_id)
    .bind(&data)
    .fetch_one(&mut *tx_a)
    .await
    .expect("claim sequence N in tx A");

    // Commit the following event (N+1) in a separate transaction.
    let (after_id, seq_after) =
        insert_committed_event(&pool, "epoch_events", stream_id, 2, "after_gap").await;
    assert!(
        seq_after > seq_n,
        "the committed event must own a later global_sequence than the in-flight one"
    );

    // Give the listener several periodic ticks to observe the gap and fence it.
    tokio::time::sleep(GapDuration::from_secs(4)).await;

    // The in-flight gap must NOT be recorded (the writer may still commit).
    let entries = event_bus
        .list_gap_timeouts(Some(&subscriber_id), false, 0, 50)
        .await
        .expect("list gap timeouts");
    assert!(
        !entries.iter().any(|e| e.skipped_sequence == seq_n as u64),
        "an in-flight gap must not be recorded as a timeout while the writer is alive"
    );

    // The checkpoint must not have advanced past the held gap.
    let checkpoint = event_bus
        .get_checkpoint(&subscriber_id)
        .await
        .expect("get checkpoint");
    assert!(
        checkpoint.is_none() || matches!(checkpoint, Some(s) if s < seq_n as u64),
        "checkpoint ({checkpoint:?}) must not advance past the in-flight gap at {seq_n}"
    );

    // Commit the in-flight writer: the gap fills.
    tx_a.commit().await.expect("commit in-flight tx A");

    // Allow the listener to deliver the now-visible event and advance.
    tokio::time::sleep(GapDuration::from_secs(2)).await;

    let state = projection_events
        .get_state(stream_id)
        .await
        .unwrap()
        .expect("Should have received events once the writer committed");
    let ids: Vec<Uuid> = state.0.iter().map(|e| e.id).collect();
    assert!(
        ids.contains(&in_flight_id),
        "the previously in-flight event must be delivered after its commit"
    );
    assert!(
        ids.contains(&after_id),
        "the event after the gap must be delivered"
    );

    let checkpoint = event_bus
        .get_checkpoint(&subscriber_id)
        .await
        .expect("get checkpoint");
    assert!(
        matches!(checkpoint, Some(s) if s >= seq_after as u64),
        "checkpoint ({checkpoint:?}) must advance past the filled gap to {seq_after}"
    );

    // Still no gap-timeout record: the gap filled, it was never skipped.
    let entries = event_bus
        .list_gap_timeouts(Some(&subscriber_id), false, 0, 50)
        .await
        .expect("list gap timeouts");
    assert!(
        !entries.iter().any(|e| e.skipped_sequence == seq_n as u64),
        "a filled gap must never be recorded as a timeout"
    );

    event_bus.shutdown().await.expect("shutdown");
}

/// G-2 / FR-6: a genuinely rolled-back gap is resolved by the fence clearing
/// (no in-flight transaction can fill it) — the surrounding events are delivered
/// well within `gap_timeout` and NO gap-timeout row is recorded.
#[tokio::test]
#[serial]
async fn test_rolled_back_gap_fence_clears_without_record() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    common::truncate_epoch_tables(&pool).await;

    // Generous gap_timeout: if a record appears it can only be the (wrong)
    // backstop, never the fence-clear path we expect.
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        gap_timeout: GapDuration::from_secs(30),
        ..Default::default()
    };
    let (event_bus, subscriber_id, projection_events) = start_fence_test_bus(&pool, config).await;

    let stream_id = Uuid::new_v4();
    let (before_id, skipped_seq, after_id) =
        create_sequence_gap(&pool, "epoch_events", stream_id).await;

    // The fence should clear far faster than the 30s gap_timeout.
    tokio::time::sleep(GapDuration::from_secs(6)).await;

    let state = projection_events
        .get_state(stream_id)
        .await
        .unwrap()
        .expect("Should have received events around the rolled-back gap");
    let ids: Vec<Uuid> = state.0.iter().map(|e| e.id).collect();
    assert!(
        ids.contains(&before_id),
        "event before the gap must be delivered"
    );
    assert!(
        ids.contains(&after_id),
        "event after the gap must be delivered via fence-clear (not the backstop)"
    );

    // No gap-timeout record: the fence cleared the gap, it is not a backstop skip.
    let entries = event_bus
        .list_gap_timeouts(Some(&subscriber_id), false, 0, 50)
        .await
        .expect("list gap timeouts");
    assert!(
        !entries.iter().any(|e| e.skipped_sequence == skipped_seq),
        "a fence-cleared (proven rolled-back) gap must NOT be recorded as a timeout"
    );

    let checkpoint = event_bus
        .get_checkpoint(&subscriber_id)
        .await
        .expect("get checkpoint");
    assert!(
        matches!(checkpoint, Some(s) if s >= skipped_seq),
        "checkpoint ({checkpoint:?}) must advance past the fence-cleared gap {skipped_seq}"
    );

    event_bus.shutdown().await.expect("shutdown");
}

/// Issue test (b) / G-3 / FR-7: when an unrelated long-running transaction pins
/// `xmin`, the fence can never clear, so the `gap_timeout` backstop fires even
/// with fencing enabled — recording a durable row and invoking `on_gap_timeout`
/// exactly once.
#[tokio::test]
#[serial]
async fn test_pinned_gap_resolves_via_backstop() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    common::truncate_epoch_tables(&pool).await;

    #[derive(Default)]
    struct RecordingGapCallback {
        infos: std::sync::Mutex<Vec<GapTimeoutInfo>>,
    }

    #[async_trait::async_trait]
    impl GapTimeoutCallback for RecordingGapCallback {
        async fn on_gap_timeout(&self, info: GapTimeoutInfo) {
            self.infos.lock().unwrap().push(info);
        }
    }

    let callback = Arc::new(RecordingGapCallback::default());

    // Start the sentinel BEFORE the gap so its xid is older than the fence
    // boundary captured at gap observation — it pins xmin indefinitely.
    let mut sentinel = pool.begin().await.expect("begin sentinel tx");
    let _sentinel_xid: i64 = sqlx::query_scalar("SELECT pg_current_xact_id()::text::bigint")
        .fetch_one(&mut *sentinel)
        .await
        .expect("assign sentinel xid to pin xmin");

    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        gap_timeout: GapDuration::from_millis(800),
        on_gap_timeout: Some(callback.clone()),
        ..Default::default()
    };
    let (event_bus, subscriber_id, _projection_events) = start_fence_test_bus(&pool, config).await;

    let stream_id = Uuid::new_v4();
    let (_before_id, skipped_seq, _after_id) =
        create_sequence_gap(&pool, "epoch_events", stream_id).await;

    // The backstop must fire and record a row despite fencing being enabled,
    // because the sentinel keeps the fence pinned.
    let entry = poll_for_gap_record(&event_bus, &subscriber_id, skipped_seq)
        .await
        .expect("backstop must record a gap-timeout row when the fence is pinned");
    assert_eq!(entry.skipped_sequence, skipped_seq);
    assert_eq!(entry.subscriber_id, subscriber_id);

    // The callback fires exactly once for the skipped sequence.
    let mut matching: Vec<GapTimeoutInfo> = Vec::new();
    for _ in 0..24 {
        matching = callback
            .infos
            .lock()
            .unwrap()
            .iter()
            .filter(|i| i.subscriber_id == subscriber_id && i.skipped_sequence == skipped_seq)
            .cloned()
            .collect();
        if !matching.is_empty() {
            break;
        }
        tokio::time::sleep(GapDuration::from_millis(250)).await;
    }
    assert_eq!(
        matching.len(),
        1,
        "on_gap_timeout must fire exactly once for the backstop skip (got {})",
        matching.len()
    );

    // Release the sentinel so it no longer pins xmin.
    sentinel.rollback().await.expect("release sentinel tx");

    event_bus.shutdown().await.expect("shutdown");
}

/// FR-8: with `snapshot_fencing = false` the resolver reverts to the legacy
/// timeout-only behaviour — a rolled-back gap is skipped after `gap_timeout` and
/// recorded, with no fence involvement.
#[tokio::test]
#[serial]
async fn test_fencing_disabled_uses_timeout() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    common::truncate_epoch_tables(&pool).await;

    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        gap_timeout: GapDuration::from_millis(500),
        snapshot_fencing: false,
        ..Default::default()
    };
    let (event_bus, subscriber_id, projection_events) = start_fence_test_bus(&pool, config).await;

    let stream_id = Uuid::new_v4();
    let (before_id, skipped_seq, after_id) =
        create_sequence_gap(&pool, "epoch_events", stream_id).await;

    let entry = poll_for_gap_record(&event_bus, &subscriber_id, skipped_seq)
        .await
        .expect("with fencing disabled, the timeout-only path must record the skipped gap");
    assert_eq!(entry.skipped_sequence, skipped_seq);
    assert!(
        entry.gap_duration_ms >= 500,
        "the recorded gap_duration_ms ({}) should be >= gap_timeout (500ms)",
        entry.gap_duration_ms
    );

    let state = projection_events
        .get_state(stream_id)
        .await
        .unwrap()
        .expect("Should have received the surrounding events");
    let ids: Vec<Uuid> = state.0.iter().map(|e| e.id).collect();
    assert!(
        ids.contains(&before_id),
        "event before the gap must be delivered"
    );
    assert!(
        ids.contains(&after_id),
        "event after the gap must be delivered once the timeout fires"
    );

    event_bus.shutdown().await.expect("shutdown");
}

/// FR-10: `PgEventStore::with_table` auto-migrates a custom events table by
/// ensuring the `txid` column (with the correct DEFAULT) exists, so inserts via
/// the store populate it. Construction against a non-existent table degrades
/// gracefully (logs a warning, no panic).
#[tokio::test]
#[serial]
async fn test_ensure_txid_column_on_custom_table() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    common::truncate_epoch_tables(&pool).await;

    // Create a custom events table mirroring epoch_events, then drop the txid
    // column so we genuinely exercise ensure_txid_column re-adding it.
    let custom_table = format!("custom_events_{}", Uuid::new_v4().simple());
    sqlx::query(&format!(
        "CREATE TABLE {custom_table} (LIKE epoch_events INCLUDING ALL)"
    ))
    .execute(&pool)
    .await
    .expect("create custom events table");
    sqlx::query(&format!(
        "ALTER TABLE {custom_table} DROP COLUMN IF EXISTS txid"
    ))
    .execute(&pool)
    .await
    .expect("drop txid from custom table");

    // with_table runs ensure_txid_column during construction.
    let bus = PgEventBus::<TestEventData>::new(
        pool.clone(),
        format!("custom_ch_{}", Uuid::new_v4().simple()),
    );
    let store = PgEventStore::with_table(pool.clone(), bus, custom_table.clone()).await;

    // The column was (re)added with the correct DEFAULT.
    let default_row: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT column_default FROM information_schema.columns \
         WHERE table_name = $1 AND column_name = 'txid'",
    )
    .bind(&custom_table)
    .fetch_optional(&pool)
    .await
    .expect("query custom table txid default");
    let (default_expr,) =
        default_row.expect("ensure_txid_column must (re)add the txid column to the custom table");
    let default_expr = default_expr.expect("the txid column must carry a DEFAULT");
    assert!(
        default_expr.contains("pg_current_xact_id"),
        "the txid DEFAULT should call pg_current_xact_id, got: {default_expr}"
    );

    // Inserting through the store populates txid via the DEFAULT.
    let stream_id = Uuid::new_v4();
    store
        .store_event(new_event(stream_id, 1, "custom_table_event"))
        .await
        .expect("store event into custom table");
    let txid: Option<i64> = sqlx::query_scalar(&format!("SELECT txid FROM {custom_table} LIMIT 1"))
        .fetch_one(&pool)
        .await
        .expect("query custom table txid");
    assert!(
        txid.is_some(),
        "an insert into the custom table must populate txid"
    );

    // Graceful degradation: with_table on a non-existent table must not panic.
    let bogus_table = format!("does_not_exist_{}", Uuid::new_v4().simple());
    let bus2 = PgEventBus::<TestEventData>::new(
        pool.clone(),
        format!("bogus_ch_{}", Uuid::new_v4().simple()),
    );
    let _store2 = PgEventStore::with_table(pool.clone(), bus2, bogus_table).await;

    // Clean up the custom table.
    sqlx::query(&format!("DROP TABLE IF EXISTS {custom_table}"))
        .execute(&pool)
        .await
        .expect("drop custom events table");
}

// ==================== Phase 2: R2 pre-loop catch-up + R4 implicit trigger ====================

/// R-2: `start_listener` runs one checkpoint-driven catch-up pass over every
/// registered subscriber *before* entering its select loop. Events committed
/// while no listener is running must be reflected in the checkpoint well under
/// the 1s `flush_interval`, without waiting for a NOTIFY or timer tick.
#[tokio::test]
#[serial]
async fn test_start_listener_catches_up_before_loop() {
    let Some((pool, event_bus)) = setup_without_listener().await else {
        return;
    };
    let event_store = PgEventStore::new(pool.clone(), event_bus.clone());

    // Register a subscriber before any events exist, so subscribe()'s own
    // catch-up processes nothing and writes no checkpoint.
    let subscriber_id = format!("projection:before-loop:{}", Uuid::new_v4());
    let projection = TestProjection::with_subscriber_id(subscriber_id.clone());
    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe projection");

    // Commit events with no listener running. The NOTIFY fires but nothing
    // consumes it; without the R2 pass the checkpoint would only advance on the
    // 1s timer tick.
    let stream_id = Uuid::new_v4();
    let n = 5u64;
    for v in 1..=n {
        event_store
            .store_event(new_event(stream_id, v, &format!("e{v}")))
            .await
            .expect("Failed to store event");
    }
    // The global_sequence counter is a standalone sequence not reset by
    // TRUNCATE, so read the actual head rather than assuming it equals `n`.
    let head: i64 =
        sqlx::query_scalar("SELECT COALESCE(MAX(global_sequence), 0) FROM epoch_events")
            .fetch_one(&pool)
            .await
            .unwrap();

    event_bus
        .start_listener()
        .await
        .expect("Failed to start listener");

    // Poll (rather than a single fixed sleep) so a loaded CI box gets extra
    // margin without slowing down the common case. `>=` rather than `==`
    // against our own `head` snapshot: the events table is shared with sibling
    // test binaries running in parallel, so the checkpoint may legitimately
    // advance past our own N events if another process inserted concurrently;
    // it must never be lower.
    let mut checkpoint = None;
    for _ in 0..20 {
        checkpoint = event_bus
            .get_checkpoint(&subscriber_id)
            .await
            .expect("Failed to read checkpoint");
        if checkpoint.unwrap_or(0) >= head as u64 {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
    }
    assert!(
        checkpoint.unwrap_or(0) >= head as u64,
        "start_listener should catch up to at least our own head ({head}) before entering \
         its loop, without waiting for the timer tick; got {checkpoint:?}"
    );

    event_bus.shutdown().await.expect("Failed to shutdown");
}

/// R-4: `start_listener` folds in an idempotent `ensure_trigger`, so an Async
/// bus started without an explicit `setup_trigger` still delivers newly
/// published events promptly (via NOTIFY, not the timer tick). Calling
/// `setup_trigger` afterwards remains harmless (idempotent).
#[tokio::test]
#[serial]
async fn test_ensure_trigger_implicit_in_start_listener() {
    let Some((pool, event_bus)) = setup_without_listener().await else {
        return;
    };
    let event_store = PgEventStore::new(pool.clone(), event_bus.clone());

    // Deliberately no setup_trigger() call.
    let subscriber_id = format!("projection:implicit-trigger:{}", Uuid::new_v4());
    let projection = TestProjection::with_subscriber_id(subscriber_id.clone());
    let store = projection.get_state_store().clone();
    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe projection");

    event_bus
        .start_listener()
        .await
        .expect("Failed to start listener");

    // Publish after the listener is up. This must be delivered via NOTIFY, which
    // requires the trigger start_listener created implicitly.
    let stream_id = Uuid::new_v4();
    let event = new_event(stream_id, 1, "implicit");
    event_store
        .store_event(event.clone())
        .await
        .expect("Failed to store event");

    // Poll well under the 1s timer tick: prompt delivery proves the trigger
    // exists. Polling (rather than one fixed sleep) avoids flaking on a loaded
    // CI box while still failing fast in the common case.
    let mut state = None;
    for _ in 0..20 {
        state = store.get_state(stream_id).await.unwrap();
        if state.is_some() {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
    }
    let state = state
        .expect("event should be delivered promptly because start_listener created the trigger");
    assert_eq!(state.0.len(), 1);
    assert_eq!(state.0[0].id, event.id);

    // Idempotent alongside an explicit setup_trigger call.
    event_bus
        .setup_trigger()
        .await
        .expect("explicit setup_trigger should be idempotent");

    event_bus.shutdown().await.expect("Failed to shutdown");
}

/// R-4: subscribing in Async mode before any trigger exists logs a WARN and
/// still succeeds (no hard error).
///
/// Runs against a dedicated database rather than the shared test database:
/// making "trigger absent" genuinely true requires dropping
/// `epoch_event_bus_notify_trigger`, a schema object other test binaries
/// running in parallel against the shared database depend on for NOTIFY
/// delivery (`#[serial]` only serializes within this binary, not across the
/// separate processes `cargo test --workspace` runs per integration-test file).
#[tokio::test]
#[serial]
async fn test_subscribe_warns_when_trigger_absent() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool_for_db("epoch_pg_test_no_trigger").await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    common::truncate_epoch_tables(&pool).await;

    let channel_name = TEST_CHANNEL.to_string();
    let event_bus = PgEventBus::new(pool.clone(), channel_name);

    // A freshly migrated database never had the trigger created on it (no
    // migration creates `epoch_event_bus_notify_trigger`; only
    // setup_trigger()/start_listener() do, at runtime), so "absent" is genuine
    // without dropping any schema object another test depends on.
    let log_start = common::captured_logs_len();

    // No setup_trigger(): the Async subscribe must warn but still succeed.
    let subscriber_id = format!("projection:no-trigger:{}", Uuid::new_v4());
    let projection = TestProjection::with_subscriber_id(subscriber_id);
    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("subscribe should succeed even without a trigger");

    assert!(
        common::captured_logs_contain_since(log_start, "no NOTIFY trigger"),
        "Async subscribe without a trigger should emit a WARN naming the missing trigger"
    );
}

// ==================== Phase 3: R5 ReplayAlways + in-memory HWM ====================

/// Reads the current head (max global_sequence) of the default events table.
async fn events_head(pool: &PgPool) -> u64 {
    let head: i64 =
        sqlx::query_scalar("SELECT COALESCE(MAX(global_sequence), 0) FROM epoch_events")
            .fetch_one(pool)
            .await
            .unwrap();
    head as u64
}

/// R-5: a ReplayAlways subscriber ignores a persisted checkpoint that a prior
/// run parked at head and still replays every event from 0. The bus never reads
/// or writes that checkpoint on its behalf.
#[tokio::test]
#[serial]
async fn test_replay_always_replays_from_zero() {
    let Some((pool, event_bus)) = setup_without_listener().await else {
        return;
    };
    let event_store = PgEventStore::new(pool.clone(), event_bus.clone());

    // Commit N events with no subscriber registered.
    let stream_id = Uuid::new_v4();
    let n = 5u64;
    for v in 1..=n {
        event_store
            .store_event(new_event(stream_id, v, &format!("e{v}")))
            .await
            .expect("Failed to store event");
    }
    let head = events_head(&pool).await;

    // Simulate a prior seed run that fast-forwarded this subscriber's checkpoint
    // to head (the CLOUD-217 shape that suppresses replay for a Checkpointed
    // subscriber).
    let subscriber_id = format!("projection:replay-always:{}", Uuid::new_v4());
    event_bus
        .update_checkpoint(&subscriber_id, head, Uuid::new_v4())
        .await
        .expect("Failed to plant checkpoint");

    // A ReplayAlways subscriber must replay all N from 0 despite the head checkpoint.
    let projection = TestProjection::replay_always(subscriber_id.clone());
    let store = projection.get_state_store().clone();
    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe ReplayAlways projection");

    let state = store
        .get_state(stream_id)
        .await
        .unwrap()
        .expect("ReplayAlways subscriber should have replayed events");
    assert_eq!(
        state.0.len(),
        n as usize,
        "ReplayAlways must replay every event from 0, ignoring the head checkpoint"
    );

    // The bus never advanced or cleared the checkpoint; the only row present is
    // the one we planted, left untouched.
    let checkpoint = event_bus
        .get_checkpoint(&subscriber_id)
        .await
        .expect("Failed to read checkpoint");
    assert_eq!(
        checkpoint,
        Some(head),
        "the bus must not read, advance, or clear a ReplayAlways subscriber's checkpoint"
    );
}

/// R-5: `fast_forward_all_subscribers` skips ReplayAlways subscribers (leaving no
/// checkpoint row) while still parking Checkpointed subscribers at head.
#[tokio::test]
#[serial]
async fn test_fast_forward_skips_replay_always() {
    let Some((pool, event_bus)) = setup_without_listener().await else {
        return;
    };
    let event_store = PgEventStore::new(pool.clone(), event_bus.clone());

    // Register both kinds before any events exist, so subscribe()'s own catch-up
    // writes nothing.
    let cp_id = format!("projection:checkpointed:{}", Uuid::new_v4());
    let ra_id = format!("projection:replay-always:{}", Uuid::new_v4());
    event_bus
        .subscribe(ProjectionHandler::new(TestProjection::with_subscriber_id(
            cp_id.clone(),
        )))
        .await
        .expect("Failed to subscribe Checkpointed projection");
    event_bus
        .subscribe(ProjectionHandler::new(TestProjection::replay_always(
            ra_id.clone(),
        )))
        .await
        .expect("Failed to subscribe ReplayAlways projection");

    // Commit events with no listener running.
    let stream_id = Uuid::new_v4();
    for v in 1..=3u64 {
        event_store
            .store_event(new_event(stream_id, v, &format!("e{v}")))
            .await
            .expect("Failed to store event");
    }
    let head = events_head(&pool).await;

    event_bus
        .fast_forward_all_subscribers()
        .await
        .expect("fast_forward failed");

    assert_eq!(
        event_bus.get_checkpoint(&cp_id).await.unwrap(),
        Some(head),
        "fast_forward must park a Checkpointed subscriber at head"
    );
    assert_eq!(
        event_bus.get_checkpoint(&ra_id).await.unwrap(),
        None,
        "fast_forward must skip a ReplayAlways subscriber (no checkpoint row written)"
    );
}

/// R-5 (Correction 2): after subscribe()'s catch-up advances the in-memory HWM,
/// the listener seeds a ReplayAlways subscriber's state from that HWM rather than
/// the (absent) checkpoint row, so the first live batch delivers only new events
/// rather than re-delivering the entire history.
#[tokio::test]
#[serial]
async fn test_replay_always_listener_does_not_redeliver_history() {
    let Some((pool, event_bus)) = setup_without_listener().await else {
        return;
    };
    let event_store = PgEventStore::new(pool.clone(), event_bus.clone());

    // Commit N events, then subscribe ReplayAlways (catch-up replays all N and
    // sets the HWM to head).
    let stream_id = Uuid::new_v4();
    let n = 4u64;
    for v in 1..=n {
        event_store
            .store_event(new_event(stream_id, v, &format!("e{v}")))
            .await
            .expect("Failed to store event");
    }

    let subscriber_id = format!("projection:replay-always:{}", Uuid::new_v4());
    let projection = TestProjection::replay_always(subscriber_id.clone());
    let store = projection.get_state_store().clone();
    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe ReplayAlways projection");

    assert_eq!(
        store.get_state(stream_id).await.unwrap().unwrap().0.len(),
        n as usize,
        "subscribe catch-up should replay all N events"
    );

    event_bus
        .start_listener()
        .await
        .expect("Failed to start listener");

    // One new event after the listener is up.
    event_store
        .store_event(new_event(stream_id, n + 1, "new"))
        .await
        .expect("Failed to store event");

    // Poll rather than a fixed sleep, for CI headroom without slowing the
    // common case; the final assert_eq still checks the exact count.
    let mut len = 0;
    for _ in 0..20 {
        len = store.get_state(stream_id).await.unwrap().unwrap().0.len();
        if len >= (n + 1) as usize {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        len,
        (n + 1) as usize,
        "listener must seed from the HWM and deliver only the new event, not re-deliver history"
    );

    event_bus.shutdown().await.expect("Failed to shutdown");
}

/// R-5 (Correction 3): a fresh subscribe of a ReplayAlways subscriber resets the
/// HWM to 0 before catch-up, so re-subscribing the same id rebuilds the model
/// from 0 rather than resuming from the prior lifecycle's HWM.
#[tokio::test]
#[serial]
async fn test_replay_always_hwm_reset_on_resubscribe() {
    let Some((pool, event_bus)) = setup_without_listener().await else {
        return;
    };
    let event_store = PgEventStore::new(pool.clone(), event_bus.clone());

    let stream_id = Uuid::new_v4();
    let n = 5u64;
    for v in 1..=n {
        event_store
            .store_event(new_event(stream_id, v, &format!("e{v}")))
            .await
            .expect("Failed to store event");
    }

    let subscriber_id = format!("projection:replay-always:{}", Uuid::new_v4());

    // First subscribe: replays all N and advances the HWM to head.
    let proj1 = TestProjection::replay_always(subscriber_id.clone());
    event_bus
        .subscribe(ProjectionHandler::new(proj1))
        .await
        .expect("Failed to subscribe ReplayAlways projection");

    // Re-subscribe the same id with a fresh model. If the HWM were NOT reset,
    // catch-up would resume from head and this projection would receive nothing.
    let proj2 = TestProjection::replay_always(subscriber_id.clone());
    let store2 = proj2.get_state_store().clone();
    event_bus
        .subscribe(ProjectionHandler::new(proj2))
        .await
        .expect("Failed to re-subscribe ReplayAlways projection");

    assert_eq!(
        store2.get_state(stream_id).await.unwrap().unwrap().0.len(),
        n as usize,
        "re-subscribe must reset the HWM to 0 and replay every event into the fresh model"
    );
}

/// R-5 (Correction 3 revised): after shutdown() + start_listener() in the same
/// process the ReplayAlways model survives, so catch-up proceeds from the
/// surviving HWM (only events missed during downtime) rather than replaying from
/// 0 into the non-empty model.
#[tokio::test]
#[serial]
async fn test_replay_always_listener_restart_from_hwm() {
    let Some((pool, event_bus)) = setup_without_listener().await else {
        return;
    };
    let event_store = PgEventStore::new(pool.clone(), event_bus.clone());

    let stream_id = Uuid::new_v4();
    let n = 4u64;
    for v in 1..=n {
        event_store
            .store_event(new_event(stream_id, v, &format!("e{v}")))
            .await
            .expect("Failed to store event");
    }

    let subscriber_id = format!("projection:replay-always:{}", Uuid::new_v4());
    let projection = TestProjection::replay_always(subscriber_id.clone());
    let store = projection.get_state_store().clone();
    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe ReplayAlways projection");

    assert_eq!(
        store.get_state(stream_id).await.unwrap().unwrap().0.len(),
        n as usize,
        "subscribe catch-up should replay all N events"
    );

    event_bus
        .start_listener()
        .await
        .expect("Failed to start listener");
    event_bus.shutdown().await.expect("Failed to shutdown");

    // Event missed during downtime.
    event_store
        .store_event(new_event(stream_id, n + 1, "missed"))
        .await
        .expect("Failed to store event");

    // Restart: the projection object (and its model) survived shutdown, so the
    // catch-up must resume from the surviving HWM and deliver only the missed
    // event, not replay from 0 into the already-built model.
    event_bus
        .start_listener()
        .await
        .expect("Failed to restart listener");

    // Poll rather than a fixed sleep, for CI headroom without slowing the
    // common case; the final assert_eq still checks the exact count.
    let mut len = 0;
    for _ in 0..20 {
        len = store.get_state(stream_id).await.unwrap().unwrap().0.len();
        if len >= (n + 1) as usize {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        len,
        (n + 1) as usize,
        "listener restart must catch up from the surviving HWM, not replay history from 0"
    );

    event_bus.shutdown().await.expect("Failed to shutdown");
}

// ---------------------------------------------------------------------------
// Phase 4 — R1 lag / readiness (R-1, R-6)
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn test_subscriber_lag_reports_behind() {
    let Some((pool, event_bus)) = setup_without_listener().await else {
        return;
    };
    let event_store = PgEventStore::new(pool.clone(), event_bus.clone());

    let subscriber_id = format!("projection:lag:{}", Uuid::new_v4());
    let projection = TestProjection::with_subscriber_id(subscriber_id.clone());
    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe");

    // Write N events without a running listener so the checkpoint stays at 0.
    let stream_id = Uuid::new_v4();
    let n = 5u64;
    for v in 1..=n {
        event_store
            .store_event(new_event(stream_id, v, &format!("e{v}")))
            .await
            .expect("store_event failed");
    }

    let lag = event_bus
        .subscriber_lag(&subscriber_id)
        .await
        .expect("subscriber_lag failed");
    assert!(
        lag >= n,
        "subscriber should be behind by at least {n} events, got lag={lag}"
    );

    // Start the listener and wait for catch-up.
    event_bus
        .setup_trigger()
        .await
        .expect("setup_trigger failed");
    event_bus
        .start_listener()
        .await
        .expect("start_listener failed");

    // R2 ensures catch-up runs before the loop, so lag should resolve quickly.
    let caught_up = event_bus
        .wait_until_caught_up(&subscriber_id, tokio::time::Duration::from_secs(3))
        .await
        .expect("wait_until_caught_up failed");
    assert!(
        caught_up,
        "subscriber should be caught up after listener start"
    );

    let lag_after = event_bus
        .subscriber_lag(&subscriber_id)
        .await
        .expect("subscriber_lag after catch-up failed");
    assert_eq!(lag_after, 0, "lag should be 0 after catch-up");

    event_bus.shutdown().await.expect("shutdown failed");
}

#[tokio::test]
#[serial]
async fn test_wait_until_caught_up_gates_on_head() {
    // Subscribe, then write events; wait_until_caught_up returns true well
    // under 1 s (proves R2 removed the flush_interval tick cost).
    let Some((pool, event_bus)) = setup_without_listener().await else {
        return;
    };
    let event_store = PgEventStore::new(pool.clone(), event_bus.clone());

    let subscriber_id = format!("projection:catchup:{}", Uuid::new_v4());
    let projection = TestProjection::with_subscriber_id(subscriber_id.clone());
    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe");

    event_bus
        .setup_trigger()
        .await
        .expect("setup_trigger failed");
    event_bus
        .start_listener()
        .await
        .expect("start_listener failed");

    let stream_id = Uuid::new_v4();
    for v in 1..=3u64 {
        event_store
            .store_event(new_event(stream_id, v, &format!("e{v}")))
            .await
            .expect("store_event failed");
    }

    // No wall-clock assertion here: a CI-loaded-box timing bound is inherently
    // flaky and belongs in a benchmark, not a correctness test. R2 removing
    // the flush_interval cost is exercised functionally instead, by the
    // `timeout` below being far shorter than the 1s tick it replaces.
    let caught_up = event_bus
        .wait_until_caught_up(&subscriber_id, tokio::time::Duration::from_millis(500))
        .await
        .expect("wait_until_caught_up failed");

    assert!(caught_up, "subscriber should be caught up");

    event_bus.shutdown().await.expect("shutdown failed");
}

#[tokio::test]
#[serial]
async fn test_wait_until_caught_up_times_out() {
    // Without a listener the checkpoint never advances; the call must return
    // Ok(false) at timeout, not hang.
    let Some((pool, event_bus)) = setup_without_listener().await else {
        return;
    };
    let event_store = PgEventStore::new(pool.clone(), event_bus.clone());

    let subscriber_id = format!("projection:timeout:{}", Uuid::new_v4());
    let projection = TestProjection::with_subscriber_id(subscriber_id.clone());
    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe");

    let stream_id = Uuid::new_v4();
    event_store
        .store_event(new_event(stream_id, 1, "stuck"))
        .await
        .expect("store_event failed");

    // Short timeout; listener is not running so position stays at 0.
    let start = std::time::Instant::now();
    let result = event_bus
        .wait_until_caught_up(&subscriber_id, tokio::time::Duration::from_millis(200))
        .await
        .expect("wait_until_caught_up should not error");
    let elapsed = start.elapsed();

    assert!(
        !result,
        "should time out (position never advances without listener)"
    );
    // Should return around the timeout, not much later.
    assert!(
        elapsed.as_millis() < 2000,
        "timed out too late: {}ms",
        elapsed.as_millis()
    );
}

#[tokio::test]
#[serial]
async fn test_replay_always_readiness_via_hwm() {
    // A ReplayAlways subscriber's lag / wait_until_caught_up resolve via the
    // in-memory HWM, never via the checkpoint table (R-6).
    let Some((pool, event_bus)) = setup_without_listener().await else {
        return;
    };
    let event_store = PgEventStore::new(pool.clone(), event_bus.clone());

    let stream_id = Uuid::new_v4();
    let n = 4u64;
    for v in 1..=n {
        event_store
            .store_event(new_event(stream_id, v, &format!("e{v}")))
            .await
            .expect("store_event failed");
    }

    let subscriber_id = format!("projection:replay-ready:{}", Uuid::new_v4());
    let projection = TestProjection::replay_always(subscriber_id.clone());
    // subscribe() triggers a catch-up pass, so the HWM is advanced to head.
    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe");

    // After subscribe, the HWM should have been advanced during catch-up.
    let lag = event_bus
        .subscriber_lag(&subscriber_id)
        .await
        .expect("subscriber_lag failed");
    assert_eq!(
        lag, 0,
        "ReplayAlways lag should be 0 after subscribe catch-up (HWM at head)"
    );

    let caught_up = event_bus
        .wait_until_caught_up(&subscriber_id, tokio::time::Duration::from_millis(500))
        .await
        .expect("wait_until_caught_up failed");
    assert!(
        caught_up,
        "ReplayAlways subscriber should be caught up immediately after subscribe"
    );

    // Confirm no checkpoint row was written (the position is purely HWM-based).
    let checkpoint = event_bus
        .get_checkpoint(&subscriber_id)
        .await
        .expect("get_checkpoint failed");
    assert!(
        checkpoint.is_none(),
        "ReplayAlways subscriber must not write a checkpoint row"
    );
}

#[tokio::test]
#[serial]
async fn test_readiness_unknown_subscriber_errors() {
    use epoch_pg::PgEventBusError;

    let Some((_pool, event_bus)) = setup_without_listener().await else {
        return;
    };

    let nonexistent = format!("projection:ghost:{}", Uuid::new_v4());

    let lag_result = event_bus.subscriber_lag(&nonexistent).await;
    assert!(
        matches!(lag_result, Err(PgEventBusError::SubscriberNotFound(_))),
        "subscriber_lag for unknown id must return SubscriberNotFound, got: {lag_result:?}"
    );

    let wait_result = event_bus
        .wait_until_caught_up(&nonexistent, tokio::time::Duration::from_millis(100))
        .await;
    assert!(
        matches!(wait_result, Err(PgEventBusError::SubscriberNotFound(_))),
        "wait_until_caught_up for unknown id must return SubscriberNotFound, got: {wait_result:?}"
    );
}

#[tokio::test]
#[serial]
async fn test_readiness_inline_dispatch_errors() {
    use epoch_pg::PgEventBusError;
    use epoch_pg::event_bus::{DispatchMode, ReliableDeliveryConfig};

    // Inline dispatch advances neither a checkpoint nor an in-memory HWM, so
    // all three readiness methods must fail fast with InlineDispatchNotSupported
    // instead of polling until timeout and reporting perpetually-not-ready.
    // The check runs before subscriber lookup, so no subscriber needs to be
    // registered to observe it.
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    common::truncate_epoch_tables(&pool).await;

    let channel_name = TEST_CHANNEL.to_string();
    let config = ReliableDeliveryConfig {
        dispatch_mode: DispatchMode::Inline,
        ..Default::default()
    };
    let event_bus: PgEventBus<TestEventData> =
        PgEventBus::with_config(pool.clone(), channel_name, config);

    let subscriber_id = format!("projection:inline-readiness:{}", Uuid::new_v4());

    let lag_result = event_bus.subscriber_lag(&subscriber_id).await;
    assert!(
        matches!(lag_result, Err(PgEventBusError::InlineDispatchNotSupported)),
        "subscriber_lag on an Inline bus must return InlineDispatchNotSupported, got: {lag_result:?}"
    );

    let wait_result = event_bus
        .wait_until_caught_up(&subscriber_id, tokio::time::Duration::from_millis(100))
        .await;
    assert!(
        matches!(
            wait_result,
            Err(PgEventBusError::InlineDispatchNotSupported)
        ),
        "wait_until_caught_up on an Inline bus must return InlineDispatchNotSupported, got: {wait_result:?}"
    );

    let wait_all_result = event_bus
        .wait_until_all_caught_up(tokio::time::Duration::from_millis(100))
        .await;
    assert!(
        matches!(
            wait_all_result,
            Err(PgEventBusError::InlineDispatchNotSupported)
        ),
        "wait_until_all_caught_up on an Inline bus must return InlineDispatchNotSupported, got: {wait_all_result:?}"
    );
}

/// An `EventObserver` whose `on_event` blocks until externally released, used
/// to hold one subscriber's checkpoint back so a gating test can observe a
/// genuine not-yet-ready state rather than trivially passing because every
/// subscriber happened to catch up immediately.
struct GatedObserver {
    subscriber_id: String,
    released_rx: tokio::sync::watch::Receiver<bool>,
    /// When present, `on_event` flips this to `true` on entry (before it blocks
    /// on the gate), so a test can wait for the observer to be genuinely parked
    /// rather than sleeping and hoping. `None` for callers that do not need it.
    entered_tx: Option<tokio::sync::watch::Sender<bool>>,
    /// Every `global_sequence` seen by `on_event`, in arrival order. Shared so
    /// the test can assert which events were actually processed after the
    /// observer has been consumed by `subscribe()`.
    seen_seqs: Arc<tokio::sync::Mutex<Vec<u64>>>,
}

impl GatedObserver {
    /// Returns the observer plus a sender the test uses to release it later.
    /// `send(true)` releases: events already blocked in `on_event` wake
    /// immediately, and any event that arrives after release sees the current
    /// value already `true` and never blocks.
    ///
    /// A `watch` channel rather than an `AtomicBool` + `Notify` pair: with
    /// `Notify::notify_waiters()`, a call that reads the flag as `false` and is
    /// preempted before it starts waiting misses the wakeup and blocks forever,
    /// since `notify_waiters()` stores no permit for waiters that register
    /// after it runs. `watch` has no such gap because the released state is
    /// itself the value being watched: checking it and waiting for it to
    /// change are the same operation, so there is no window in which a change
    /// can happen invisibly between them.
    fn new(subscriber_id: String) -> (Self, tokio::sync::watch::Sender<bool>) {
        let (tx, rx) = tokio::sync::watch::channel(false);
        (
            Self {
                subscriber_id,
                released_rx: rx,
                entered_tx: None,
                seen_seqs: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            },
            tx,
        )
    }

    /// Like [`GatedObserver::new`] but also returns a receiver that flips to
    /// `true` the moment `on_event` is first entered, letting a test observe
    /// that catch-up is provably parked on an event before it acts.
    fn new_with_entry_signal(
        subscriber_id: String,
    ) -> (
        Self,
        tokio::sync::watch::Sender<bool>,
        tokio::sync::watch::Receiver<bool>,
    ) {
        let (released_tx, released_rx) = tokio::sync::watch::channel(false);
        let (entered_tx, entered_rx) = tokio::sync::watch::channel(false);
        (
            Self {
                subscriber_id,
                released_rx,
                entered_tx: Some(entered_tx),
                seen_seqs: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            },
            released_tx,
            entered_rx,
        )
    }

    fn seen_seqs(&self) -> Arc<tokio::sync::Mutex<Vec<u64>>> {
        Arc::clone(&self.seen_seqs)
    }
}

impl epoch_core::SubscriberId for GatedObserver {
    fn subscriber_id(&self) -> &str {
        &self.subscriber_id
    }
}

#[async_trait]
impl EventObserver<TestEventData> for GatedObserver {
    async fn on_event(
        &self,
        event: Arc<Event<TestEventData>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let Some(seq) = event.global_sequence {
            self.seen_seqs.lock().await.push(seq);
        }
        if let Some(entered_tx) = &self.entered_tx {
            let _ = entered_tx.send(true);
        }
        let mut rx = self.released_rx.clone();
        while !*rx.borrow() {
            if rx.changed().await.is_err() {
                break; // Sender dropped; nothing left to wait for.
            }
        }
        Ok(())
    }
}

#[tokio::test]
#[serial]
async fn test_wait_until_all_caught_up_gates_every_subscriber() {
    // Two subscribers, one shared head snapshot: wait_until_all_caught_up must
    // check EVERY subscriber, not short-circuit on the first. Subscriber B is
    // gated so it provably cannot have caught up yet, proving the negative
    // case; releasing it then proves the positive one.
    let Some((pool, event_bus)) = setup_without_listener().await else {
        return;
    };
    let event_store = PgEventStore::new(pool.clone(), event_bus.clone());

    let sub_a = format!("projection:all-a:{}", Uuid::new_v4());
    let sub_b = format!("projection:all-b:{}", Uuid::new_v4());
    event_bus
        .subscribe(ProjectionHandler::new(TestProjection::with_subscriber_id(
            sub_a.clone(),
        )))
        .await
        .expect("Failed to subscribe A");
    let (gated_b, released) = GatedObserver::new(sub_b.clone());
    event_bus
        .subscribe(gated_b)
        .await
        .expect("Failed to subscribe B");

    event_bus
        .setup_trigger()
        .await
        .expect("setup_trigger failed");
    event_bus
        .start_listener()
        .await
        .expect("start_listener failed");

    let stream_id = Uuid::new_v4();
    for v in 1..=3u64 {
        event_store
            .store_event(new_event(stream_id, v, &format!("e{v}")))
            .await
            .expect("store_event failed");
    }

    // B is blocked in on_event and cannot advance its checkpoint: this must be
    // false, proving the gate actually checks B rather than short-circuiting
    // on A alone. Wrapped in an outer timeout: this is the only regression test
    // for 449a769 (readiness resolving subscriber mode from a registry rather
    // than locking each observer), and without that fix this call can block
    // forever behind B's held observer lock rather than honouring its own
    // 300ms deadline, wedging the whole suite instead of failing it.
    let all_caught_up = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        event_bus.wait_until_all_caught_up(tokio::time::Duration::from_millis(300)),
    )
    .await
    .expect("wait_until_all_caught_up blocked behind subscriber B's held observer lock")
    .expect("wait_until_all_caught_up failed");
    assert!(
        !all_caught_up,
        "must be false while subscriber B is still blocked on the gate"
    );
    // A's own position is deliberately NOT asserted here. Subscribers in a
    // priority group are processed together under one `join_all`, so the batch
    // loop cannot start the next batch until every subscriber in the group
    // finishes: while B is gated, A can only be as far as the last batch that
    // completed, which depends on how the writes happened to be split across
    // NOTIFY wakeups. Asserting `lag(A) == 0` here passes or fails on that
    // timing, not on the behaviour under test.

    // Release B; both must now be reported caught up.
    released
        .send(true)
        .expect("GatedObserver's receiver was dropped");

    let all_caught_up = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        event_bus.wait_until_all_caught_up(tokio::time::Duration::from_secs(3)),
    )
    .await
    .expect("wait_until_all_caught_up blocked past its own timeout after releasing B")
    .expect("wait_until_all_caught_up failed");
    assert!(
        all_caught_up,
        "both subscribers should be caught up after releasing B"
    );

    assert_eq!(
        event_bus.subscriber_lag(&sub_a).await.expect("lag A"),
        0,
        "subscriber A lag should be 0"
    );
    assert_eq!(
        event_bus.subscriber_lag(&sub_b).await.expect("lag B"),
        0,
        "subscriber B lag should be 0"
    );

    event_bus.shutdown().await.expect("shutdown failed");
}

/// A projection that relies entirely on `Projection`'s **defaulted**
/// `subscription_mode()` (never overridden) — the "opt-out" observer shape
/// that predates `SubscriptionMode` entirely. Proves R-7: such an observer
/// resolves to `Checkpointed` and behaves byte-for-byte identically to one
/// that explicitly sets `SubscriptionMode::Checkpointed`.
struct DefaultModeProjection {
    state_store: InMemoryStateStore<TestState>,
    subscriber_id: String,
}

impl DefaultModeProjection {
    fn new(subscriber_id: String) -> Self {
        Self {
            state_store: InMemoryStateStore::new(),
            subscriber_id,
        }
    }
}

impl epoch_core::SubscriberId for DefaultModeProjection {
    fn subscriber_id(&self) -> &str {
        &self.subscriber_id
    }
}

impl EventApplicator<TestEventData> for DefaultModeProjection {
    type State = TestState;
    type StateStore = InMemoryStateStore<Self::State>;
    type EventType = TestEventData;
    type ApplyError = TestProjectionError;

    fn get_state_store(&self) -> Self::StateStore {
        self.state_store.clone()
    }
    fn apply(
        &self,
        state: Option<Self::State>,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::ApplyError> {
        if let Some(mut state) = state {
            state.0.push(event.clone());
            Ok(Some(state))
        } else {
            Ok(Some(TestState(vec![event.clone()])))
        }
    }
}

// Deliberately no `subscription_mode()` override: this is the point of R-7.
impl Projection<TestEventData> for DefaultModeProjection {}

#[tokio::test]
#[serial]
async fn test_no_config_subscriber_identical_baseline() {
    let Some((pool, event_bus)) = setup_without_listener().await else {
        return;
    };
    let event_store = PgEventStore::new(pool.clone(), event_bus.clone());

    let subscriber_id = format!("projection:default-mode:{}", Uuid::new_v4());
    let projection = DefaultModeProjection::new(subscriber_id.clone());
    let store = projection.get_state_store().clone();
    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe");

    event_bus
        .setup_trigger()
        .await
        .expect("setup_trigger failed");
    event_bus
        .start_listener()
        .await
        .expect("start_listener failed");

    let stream_id = Uuid::new_v4();
    let n = 3u64;
    for v in 1..=n {
        event_store
            .store_event(new_event(stream_id, v, &format!("e{v}")))
            .await
            .expect("store_event failed");
    }

    // Same readiness contract as an explicit Checkpointed subscriber: gates on
    // the persisted checkpoint, resolves once caught up.
    let caught_up = event_bus
        .wait_until_caught_up(&subscriber_id, tokio::time::Duration::from_secs(3))
        .await
        .expect("wait_until_caught_up failed");
    assert!(
        caught_up,
        "defaulted-mode subscriber should be gateable exactly like Checkpointed"
    );

    assert_eq!(
        store.get_state(stream_id).await.unwrap().unwrap().0.len(),
        n as usize,
        "defaulted-mode subscriber should receive every event, same as Checkpointed"
    );

    let checkpoint = event_bus
        .get_checkpoint(&subscriber_id)
        .await
        .expect("get_checkpoint failed");
    assert!(
        checkpoint.is_some(),
        "defaulted-mode subscriber must write a checkpoint row like Checkpointed, unlike ReplayAlways"
    );

    event_bus.shutdown().await.expect("shutdown failed");
}

#[tokio::test]
#[serial]
async fn test_readiness_not_blocked_by_in_progress_drain() {
    // CLOUD-225: the listener must not hold the `projections` lock across its
    // batch drain. Every readiness API locks `projections` too, so a long-held
    // guard makes them block for the whole drain and silently ignore their own
    // timeout.
    //
    // A gated subscriber parks the listener inside its batch loop indefinitely.
    // While it is parked, readiness calls for a DIFFERENT subscriber must still
    // answer. Before the fix they blocked until the gated observer was released,
    // so the `timeout(..)` wrappers below expired and this test failed.
    let Some((pool, event_bus)) = setup_without_listener().await else {
        return;
    };
    let event_store = PgEventStore::new(pool.clone(), event_bus.clone());

    let sub_free = format!("projection:drain-free:{}", Uuid::new_v4());
    let sub_gated = format!("projection:drain-gated:{}", Uuid::new_v4());
    event_bus
        .subscribe(ProjectionHandler::new(TestProjection::with_subscriber_id(
            sub_free.clone(),
        )))
        .await
        .expect("Failed to subscribe free subscriber");
    let (gated, released) = GatedObserver::new(sub_gated.clone());
    event_bus
        .subscribe(gated)
        .await
        .expect("Failed to subscribe gated subscriber");

    event_bus
        .setup_trigger()
        .await
        .expect("setup_trigger failed");
    event_bus
        .start_listener()
        .await
        .expect("start_listener failed");

    let stream_id = Uuid::new_v4();
    event_store
        .store_event(new_event(stream_id, 1, "parks-the-listener"))
        .await
        .expect("store_event failed");

    // Wait until the listener is genuinely parked in the gated observer's
    // on_event, rather than sleeping a fixed amount and hoping. Both
    // subscribers share one priority group, so they run in the same concurrent
    // batch: once the free subscriber's checkpoint lands (Synchronous mode
    // writes it inside the batch task), the batch is provably in flight and the
    // gated task provably has not returned.
    let mut parked = false;
    for _ in 0..40 {
        if event_bus
            .get_checkpoint(&sub_free)
            .await
            .expect("get_checkpoint failed")
            .is_some_and(|c| c >= 1)
        {
            parked = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    assert!(
        parked,
        "the free subscriber should have processed the event while the gated one blocks"
    );

    // The listener is now inside its batch loop with the gated subscriber
    // blocked. These must still answer.
    let lag = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        event_bus.subscriber_lag(&sub_free),
    )
    .await
    .expect("subscriber_lag blocked behind the listener's in-progress drain")
    .expect("subscriber_lag returned an error");
    assert_eq!(lag, 0, "the free subscriber processed the only event");

    let caught_up = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        event_bus.wait_until_caught_up(&sub_free, std::time::Duration::from_millis(200)),
    )
    .await
    .expect("wait_until_caught_up blocked behind the listener's in-progress drain")
    .expect("wait_until_caught_up returned an error");
    assert!(caught_up, "the free subscriber is at head");

    // Release the gated subscriber so the listener can finish and shut down.
    released
        .send(true)
        .expect("GatedObserver's receiver was dropped");

    event_bus.shutdown().await.expect("shutdown failed");
}

#[tokio::test]
#[serial]
async fn test_second_bus_on_same_table_gets_its_own_trigger() {
    // The NOTIFY channel is an argument to the trigger, so a single shared
    // trigger name per table means a second bus either steals the first bus's
    // trigger or, if creation is skipped because some trigger already exists, is
    // left deaf and silently degraded to the periodic timer tick. Trigger names
    // are per-channel so both buses coexist, each notifying its own channel.
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    common::truncate_epoch_tables(&pool).await;

    // Two independent buses on the same events table, each with its own channel.
    let first_channel = format!("{TEST_CHANNEL}_multibus_a");
    let first_bus: PgEventBus<TestEventData> = PgEventBus::new(pool.clone(), first_channel.clone());
    first_bus
        .start_listener()
        .await
        .expect("first start_listener failed");

    let second_channel = format!("{TEST_CHANNEL}_multibus_b");
    let second_bus: PgEventBus<TestEventData> =
        PgEventBus::new(pool.clone(), second_channel.clone());

    let subscriber_id = format!("projection:rebind:{}", Uuid::new_v4());
    let projection = TestProjection::with_subscriber_id(subscriber_id.clone());
    second_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe on second bus");
    second_bus
        .start_listener()
        .await
        .expect("second start_listener failed");

    // Both buses' triggers must be present simultaneously, each bound to its own
    // channel: the second bus must not have displaced the first.
    let bound: Vec<(String,)> = sqlx::query_as(
        r#"
        SELECT split_part(encode(t.tgargs, 'escape'), E'\\000', 1) AS channel
        FROM pg_trigger t
        JOIN pg_class c ON c.oid = t.tgrelid
        WHERE t.tgname LIKE 'epoch_event_bus_notify_trigger%'
          AND c.relname = 'epoch_events'
        "#,
    )
    .fetch_all(&pool)
    .await
    .expect("failed to read trigger definitions");
    let channels: Vec<String> = bound.into_iter().map(|(c,)| c).collect();
    assert!(
        channels.contains(&second_channel),
        "the second bus must have its own trigger; bound channels: {channels:?}"
    );
    assert!(
        channels.contains(&first_channel),
        "the first bus's trigger must survive the second bus starting; bound channels: {channels:?}"
    );

    // And delivery must actually work on the second bus, promptly (i.e. via
    // NOTIFY, not by waiting out the 1s timer tick). `flush_interval` is 1s with
    // arbitrary phase relative to this write, so a 900ms deadline would be
    // satisfied by a plain timer tick most of the time and prove little; 200ms
    // is meaningfully sub-tick and actually discriminates NOTIFY delivery from
    // the fallback. The `pg_trigger` assertions above are the real proof that
    // c53e6c6's per-channel trigger exists; this only adds that it fires.
    let event_store = PgEventStore::new(pool.clone(), second_bus.clone());
    let stream_id = Uuid::new_v4();
    event_store
        .store_event(new_event(stream_id, 1, "delivered-via-notify"))
        .await
        .expect("store_event failed");

    // 500ms, not 200ms: the budget has to cover NOTIFY dispatch, a batch SELECT on
    // a table other tests are concurrently TRUNCATEing, the handler, a synchronous
    // checkpoint upsert, and up to one 25ms readiness poll. Checkpoint writes in
    // this suite have been measured stalling for seconds under load. 500ms is still
    // well under the 1s timer tick, which is all this assertion needs.
    let caught_up = second_bus
        .wait_until_caught_up(&subscriber_id, std::time::Duration::from_millis(500))
        .await
        .expect("wait_until_caught_up failed");
    assert!(
        caught_up,
        "second bus must receive its own NOTIFYs rather than depending on the timer tick"
    );

    second_bus.shutdown().await.expect("second shutdown failed");
    first_bus.shutdown().await.expect("first shutdown failed");
}

// ============================================================================
// Spec 0026 / CLOUD-226: contiguous catch-up checkpoint integration tests
//
// These exercise the two catch-up writers end-to-end against a live Postgres
// while a hole in `global_sequence` is held open by an uncommitted transaction.
// All assertions are relative to sequences captured via `INSERT ... RETURNING
// global_sequence`; absolute values are unusable because `epoch_events` and its
// sequence are shared by five parallel test binaries (spec §6). Both use stable
// channel names so they do not leak a fresh NOTIFY trigger per run (CLOUD-230).
// ============================================================================

/// Claims the next `global_sequence` inside `tx` without committing, producing a
/// permanent-until-commit hole no other transaction can fill (the value is
/// allocated to `tx`). Returns `(event_id, held_sequence)`.
///
/// `table` is the events table the hole is claimed on. On an isolated table the
/// value comes from *that* table's `DEFAULT nextval`, so the hole burns the
/// private sequence and is visible to a bus pointed at that table; passing
/// `"epoch_events"` reproduces the original shared-table behaviour.
async fn claim_hole_uncommitted(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
    stream_id: Uuid,
) -> (Uuid, i64) {
    let id = Uuid::new_v4();
    let data = serde_json::to_value(Some(TestEventData::TestEvent {
        value: "held_hole".to_string(),
    }))
    .unwrap();
    let seq: i64 = sqlx::query_scalar(&format!(
        r#"INSERT INTO {table} (id, stream_id, stream_version, event_type, data, created_at)
           VALUES ($1, $2, 1, 'MyEvent', $3, NOW())
           RETURNING global_sequence"#
    ))
    .bind(id)
    .bind(stream_id)
    .bind(&data)
    .fetch_one(&mut **tx)
    .await
    .expect("claim held sequence in tx");
    (id, seq)
}

/// Creates a table shaped like `epoch_events` backed by its OWN sequence for
/// `global_sequence`, so a test can burn a `nextval()` (or hold one open in an
/// uncommitted transaction) to simulate a hole without leaving one on the shared
/// `epoch_events` table, where it would stall every sibling binary's catch-up
/// and live loop.
///
/// This is the integration-binary copy of `cu_isolated_events_table` from
/// `event_bus/mod.rs` (that helper is `#[cfg(test)]` and not reachable from
/// `tests/`). `LIKE ... INCLUDING ALL` copies the DEFAULT expression
/// byte-for-byte (still pointing at the shared sequence) and is then repointed
/// at the fresh, `OWNED BY` sequence; it does NOT copy triggers, so a bus using
/// this table must run `setup_trigger()` before `start_listener()`, with a
/// `Uuid`-unique channel name. The caller must drop the returned table when
/// done (dropping it drops the owned sequence too).
async fn isolated_events_table(pool: &PgPool) -> String {
    let table = format!("eb_isolated_events_{}", Uuid::new_v4().simple());
    let seq = format!("{table}_seq");
    sqlx::query(&format!(
        "CREATE TABLE {table} (LIKE epoch_events INCLUDING ALL)"
    ))
    .execute(pool)
    .await
    .expect("create isolated events table");
    sqlx::query(&format!("CREATE SEQUENCE {seq}"))
        .execute(pool)
        .await
        .expect("create isolated sequence");
    sqlx::query(&format!(
        "ALTER TABLE {table} ALTER COLUMN global_sequence SET DEFAULT nextval('{seq}')"
    ))
    .execute(pool)
    .await
    .expect("repoint isolated table's global_sequence default");
    sqlx::query(&format!(
        "ALTER SEQUENCE {seq} OWNED BY {table}.global_sequence"
    ))
    .execute(pool)
    .await
    .expect("bind isolated sequence ownership");
    table
}

/// Drops an isolated events table created by [`isolated_events_table`]. Its
/// `OWNED BY` sequence is dropped with it.
async fn drop_isolated_events_table(pool: &PgPool, table: &str) {
    sqlx::query(&format!("DROP TABLE IF EXISTS {table}"))
        .execute(pool)
        .await
        .expect("drop isolated events table");
}

/// Test 4 (R1, R5): an event committed after a catch-up pass that ran over a
/// held hole is still delivered, and readiness only reports caught-up once the
/// hole resolves.
///
/// The subscriber is registered *before* `start_listener`, so the R2 pre-loop
/// catch-up pass (the once-per-boot writer that motivated this fix) is what runs
/// over the hole. Readiness uses a bounded `wait_until_caught_up`, never a fixed
/// sleep. The persisted checkpoint is pre-planted just below the hole so the
/// pass is isolated from unrelated history left by sibling test binaries on the
/// shared table.
#[tokio::test]
#[serial]
async fn test_catchup_hole_delivers_after_commit_and_reports_ready() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    common::truncate_epoch_tables(&pool).await;

    // gap_timeout comfortably longer than the ~800ms window we hold the hole for
    // the not-ready observation, so the backstop cannot skip our hole before we
    // commit it; short enough that the final wait tolerates an unrelated
    // permanent hole from a sibling binary being backstop-skipped.
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        gap_timeout: GapDuration::from_secs(2),
        ..Default::default()
    };
    let channel_name = "test_catchup_e2e".to_string();
    let event_bus = PgEventBus::<TestEventData>::with_config(pool.clone(), channel_name, config);
    event_bus
        .setup_trigger()
        .await
        .expect("Failed to setup trigger");

    let stream_id = Uuid::new_v4();

    // Hold a hole open, then commit an event above it.
    let mut tx_a = pool.begin().await.expect("begin in-flight tx A");
    let (in_flight_id, seq_n) = claim_hole_uncommitted(&mut tx_a, "epoch_events", stream_id).await;
    let (after_id, seq_after) =
        insert_committed_event(&pool, "epoch_events", stream_id, 2, "after_hole").await;
    assert!(
        seq_after > seq_n,
        "the committed event must own a later global_sequence than the held hole"
    );

    let subscriber_id = format!("projection:catchup-e2e:{}", Uuid::new_v4());
    let projection = TestProjection::with_subscriber_id(subscriber_id.clone());
    let projection_events = projection.get_state_store().clone();

    // Plant the checkpoint just below the hole so catch-up only walks our own
    // events, not the shared table's accumulated history. The Uuid::new_v4()
    // event_id does not correspond to a real event — deliberate throwaway seed
    // state that gets overwritten once the hole fills and catch-up runs.
    event_bus
        .update_checkpoint(&subscriber_id, seq_n as u64 - 1, Uuid::new_v4())
        .await
        .expect("plant checkpoint below hole");

    // Register before start_listener so the R2 pre-loop pass covers this
    // subscriber (subscribing after would run the loop with no subscribers, and
    // exercise subscribe()'s own catch-up instead).
    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe");

    event_bus
        .start_listener()
        .await
        .expect("Failed to start listener");

    // R5: while the hole is held, readiness must NOT report caught-up. The head
    // is at least `seq_after`, but the checkpoint is legitimately pinned below
    // the hole, so the honest answer is "not yet".
    //
    // Both observations must be made while the hole is still held, so capture
    // them and assert *after* committing. Asserting here would unwind with the
    // transaction open, and an open transaction on epoch_events blocks a
    // sibling binary's TRUNCATE, whose pending ACCESS EXCLUSIVE then queues
    // ahead of every later reader and writer on the shared table.
    let caught_up_while_held = event_bus
        .wait_until_caught_up(&subscriber_id, GapDuration::from_millis(800))
        .await
        .expect("wait_until_caught_up failed");
    let checkpoint_while_held = event_bus
        .get_checkpoint(&subscriber_id)
        .await
        .expect("get checkpoint");

    // Fill the hole.
    tx_a.commit().await.expect("commit in-flight tx A");

    assert!(
        !caught_up_while_held,
        "readiness must block, not report caught-up, while the checkpoint is held below the hole"
    );
    assert!(
        matches!(checkpoint_while_held, Some(s) if s < seq_n as u64),
        "checkpoint ({checkpoint_while_held:?}) must stay below the held hole at {seq_n}"
    );

    // Now readiness resolves via a bounded wait (not a fixed sleep). Generous
    // bound: the shared table may carry unrelated holes the backstop skips one
    // gap_timeout apart before this subscriber reaches the head snapshot.
    let caught_up = event_bus
        .wait_until_caught_up(&subscriber_id, GapDuration::from_secs(15))
        .await
        .expect("wait_until_caught_up failed");
    assert!(
        caught_up,
        "readiness must report caught-up once the hole resolves"
    );

    // Both the previously-held event and the one above it must be delivered.
    let state = projection_events
        .get_state(stream_id)
        .await
        .unwrap()
        .expect("subscriber should have received events once the hole filled");
    let ids: Vec<Uuid> = state.0.iter().map(|e| e.id).collect();
    assert!(
        ids.contains(&in_flight_id),
        "the previously-held event must be delivered after its commit"
    );
    assert!(
        ids.contains(&after_id),
        "the event above the hole must be delivered"
    );

    event_bus.shutdown().await.expect("shutdown");
}

/// Test 5 (R2, subscribe drain path): `subscribe()`'s buffer-drain pass must
/// not flush a checkpoint above a held hole, even when it drains above-hole
/// events that catch-up never saw. This is the test that would have caught the
/// second writer (spec §6 test 5).
///
/// The drain reuses catch-up's `contiguous` prefix counter and must flush only
/// that value. Because `flush_checkpoint` is a blind, non-monotonic upsert, a
/// drain that flushed its own linear maximum would overwrite catch-up's
/// conservative checkpoint inside the same `subscribe()` call.
///
/// Giving the drain power over R2 requires a buffered event strictly *above*
/// catch-up's returned pagination cursor, i.e. one catch-up never saw. Rather
/// than race a concurrent writer against catch-up (only load-sensitively
/// reliable), this drives the boundary deterministically with a gated observer:
///
///   1. Two above-hole events pre-exist. With the default `catch_up_batch_size`
///      (100) they form catch-up's single short page, so catch-up breaks after
///      processing them **without a re-query**.
///   2. The gated observer parks catch-up on the first of those two events —
///      after its page query has run — and signals that it is parked.
///   3. A third above-hole event is committed while catch-up is parked. Its
///      NOTIFY is buffered, but catch-up never re-queries, so it lands strictly
///      above the pagination cursor and the drain must process it.
///
/// The persisted checkpoint is read immediately, while the hole is still held,
/// so the drain's flush is the only thing that could have moved it. Catch-up
/// itself leaves the checkpoint at the planted `seq_n - 1` (its contiguous
/// prefix never advances past the hole and its flush is skipped), so a checkpoint
/// at or above `seq_n` can only come from the drain.
///
/// Reverting the Phase 2 drain fix makes this fail: the pre-fix drain advanced
/// a linear `PendingCheckpoint` per drained row and flushed its maximum, pushing
/// the checkpoint past the hole (acceptance §9.2).
#[tokio::test]
#[serial]
async fn test_subscribe_drain_does_not_flush_above_held_hole() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    common::truncate_epoch_tables(&pool).await;

    // Default catch_up_batch_size (100) so the two pre-existing events are
    // catch-up's single short page and it breaks without a re-query. Generous
    // gap_timeout: we read the checkpoint immediately after subscribe() returns
    // while the hole is still held, so the live path's backstop must not skip
    // the hole in the meantime.
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        gap_timeout: GapDuration::from_secs(30),
        ..Default::default()
    };
    let channel_name = "test_catchup_drain".to_string();
    let event_bus = PgEventBus::<TestEventData>::with_config(pool.clone(), channel_name, config);
    event_bus
        .setup_trigger()
        .await
        .expect("Failed to setup trigger");
    // Start the listener first so subscribe() takes the buffer/drain path.
    event_bus
        .start_listener()
        .await
        .expect("Failed to start listener");

    let stream_id = Uuid::new_v4();

    // Hold a hole open (claim_hole_uncommitted uses stream_version 1), then
    // commit two events above it. Catch-up fetches both in its single page,
    // parks on the first via the gated observer, and its contiguous prefix
    // stays at seq_n - 1 (the hole is never seen).
    let mut tx_a = pool.begin().await.expect("begin in-flight tx A");
    let (_hole_id, seq_n) = claim_hole_uncommitted(&mut tx_a, "epoch_events", stream_id).await;
    let (_above1_id, seq1) =
        insert_committed_event(&pool, "epoch_events", stream_id, 2, "above1").await;
    let (_above2_id, seq2) =
        insert_committed_event(&pool, "epoch_events", stream_id, 3, "above2").await;
    assert!(
        seq1 > seq_n && seq2 > seq1,
        "the pre-existing events must sit above the held hole"
    );

    let subscriber_id = format!("projection:catchup-drain:{}", Uuid::new_v4());
    let (gated, released, mut entered_rx) =
        GatedObserver::new_with_entry_signal(subscriber_id.clone());

    // Plant the checkpoint just below the hole so catch-up walks only above-hole
    // events, isolating the assertion from shared-table history.
    event_bus
        .update_checkpoint(&subscriber_id, seq_n as u64 - 1, Uuid::new_v4())
        .await
        .expect("plant checkpoint below hole");

    // Run subscribe() concurrently: it parks inside catch-up on the first event
    // until we release the gate.
    let seen_seqs = gated.seen_seqs();
    let bus_for_task = event_bus.clone();
    let subscribe_task = tokio::spawn(async move { bus_for_task.subscribe(gated).await });

    // Wait until catch-up is provably parked on the first event. Its page query
    // (fetching both pre-existing events) has run by now, so anything committed
    // from here on is invisible to catch-up.
    tokio::time::timeout(tokio::time::Duration::from_secs(10), async {
        while !*entered_rx.borrow() {
            entered_rx
                .changed()
                .await
                .expect("entered signal sender dropped");
        }
    })
    .await
    .expect("catch-up did not reach the gated observer");

    // Let the buffer listener finish connecting and LISTENing before we commit
    // the buffered event: a NOTIFY is only delivered to sessions already
    // listening at commit time. Safe as a fixed wait because the gate holds
    // catch-up parked — this is a lower bound on connect latency, not a race.
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    // Commit a third above-hole event while catch-up is parked. Its NOTIFY is
    // buffered, but catch-up breaks after its single short page (no re-query),
    // so it lands strictly above the pagination cursor: the drain must process
    // it (spec §4.3, R2).
    let (_above3_id, seq3) =
        insert_committed_event(&pool, "epoch_events", stream_id, 4, "buffered_above").await;
    assert!(
        seq3 > seq2,
        "the buffered event must sit above the pre-existing events"
    );

    // Let the buffered NOTIFY reach the buffer channel before releasing catch-up
    // to drain it. Again a safe fixed wait: the gate still holds catch-up.
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    // Release catch-up; it processes the two pre-existing events, breaks, then
    // drains the buffered above-hole event.
    released
        .send(true)
        .expect("GatedObserver's receiver was dropped");
    subscribe_task
        .await
        .expect("subscribe task panicked")
        .expect("Failed to subscribe");

    // R2: the drain processed an above-hole event, but must not have flushed a
    // checkpoint above the still-held hole. Read immediately, before filling it.
    let checkpoint = event_bus
        .get_checkpoint(&subscriber_id)
        .await
        .expect("get checkpoint");

    // Fill the hole and shut down *before* asserting so a failing revert-check
    // (spec §9.2) does not leave the held transaction or the listener alive and
    // hang the test process on exit.
    tx_a.commit().await.expect("commit in-flight tx A");
    event_bus.shutdown().await.expect("shutdown");

    assert!(
        matches!(checkpoint, Some(s) if s < seq_n as u64),
        "checkpoint ({checkpoint:?}) must stay below the held hole at {seq_n} after subscribe()"
    );
    // The drain must have actually delivered the buffered event. If the NOTIFY
    // was missed (buffer listener not yet LISTENing at commit time), nothing is
    // drained and the checkpoint stays put — the R2 assertion above passes but
    // proves nothing. Asserting seq3 was delivered converts a silent false-pass
    // into an explicit failure.
    let seen = seen_seqs.lock().await;
    assert!(
        seen.contains(&(seq3 as u64)),
        "drain must have delivered the buffered event (seq {seq3}); actually saw: {seen:?}"
    );
}

// ============================================================================
// Spec 0027 / CLOUD-232: live-path contiguous checkpoint positive controls
//
// These pin the *other* direction of the §3.3 invariant: the persisted
// checkpoint may lag `contiguous_checkpoint`, but that lag must eventually
// close. A fix that stops the leak by simply never advancing would satisfy the
// regression tests yet stall every subscriber; these controls turn that
// failure red. They run on an isolated events table with its own sequence so a
// permanent hole can be planted without stalling sibling binaries.
//
// Pre-fix (P1) these pass because max-seen already leaks a value at or above
// the target; their real value is as a post-fix tripwire once P2 lands.
// ============================================================================

/// R2 positive control, MANDATORY and with zero load sensitivity: a hole
/// abandoned by a rolled-back transaction is skipped via the `gap_timeout`
/// backstop and the checkpoint advances past it. Depends only on elapsed
/// wall-clock (`snapshot_fencing: false`, `gap_timeout: 500ms`), so it cannot
/// flake under the instance-wide `xmin` pressure that afflicts the fence route.
#[tokio::test]
#[serial]
async fn test_backstop_hole_still_advances_checkpoint() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

    let table = isolated_events_table(&pool).await;

    // Legacy timeout-only path (snapshot_fencing: false) so the backstop both
    // skips the hole and records a gap-timeout row; short gap_timeout so the
    // skip fires quickly.
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        snapshot_fencing: false,
        gap_timeout: GapDuration::from_millis(500),
        events_table: table.clone(),
        ..Default::default()
    };

    let channel_name = format!("test_backstop_{}", Uuid::new_v4().simple());
    let event_bus = PgEventBus::<TestEventData>::with_config(pool.clone(), channel_name, config);
    event_bus
        .setup_trigger()
        .await
        .expect("Failed to setup trigger");
    event_bus
        .start_listener()
        .await
        .expect("Failed to start listener");

    let subscriber_id = format!("projection:backstop:{}", Uuid::new_v4());
    let projection = TestProjection::with_subscriber_id(subscriber_id.clone());
    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Permanent hole on the isolated sequence: event below, rolled-back hole,
    // event above. On a private sequence with no concurrent writer the above
    // event sits exactly one slot past the hole. Created after subscribe() so
    // the live listener path (not catch-up) is what walks over the hole.
    let stream_id = Uuid::new_v4();
    let (_before_id, skipped_seq, _after_id) = create_sequence_gap(&pool, &table, stream_id).await;
    let seq_above = skipped_seq + 1;

    // Poll (not sleep) until the backstop skips the hole and advances the
    // checkpoint to at least the above-hole event. Bounded well beyond
    // gap_timeout (500ms) + the hard-coded 1s flush_interval.
    let mut checkpoint = None;
    for _ in 0..40 {
        checkpoint = event_bus
            .get_checkpoint(&subscriber_id)
            .await
            .expect("get checkpoint");
        if matches!(checkpoint, Some(s) if s >= seq_above) {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    }

    let gap_records = event_bus
        .list_gap_timeouts(Some(&subscriber_id), false, 0, 50)
        .await
        .expect("Failed to list gap timeouts");

    event_bus.shutdown().await.expect("shutdown");
    drop_isolated_events_table(&pool, &table).await;

    // `>=`, not `==`: after a backstop skip the prefix legitimately runs to head.
    assert!(
        matches!(checkpoint, Some(s) if s >= seq_above),
        "backstop must advance the checkpoint ({checkpoint:?}) to at least the \
         above-hole event ({seq_above}); a fix that never advances stalls here"
    );
    assert!(
        gap_records
            .iter()
            .any(|e| e.skipped_sequence == skipped_seq),
        "the backstop must record a gap-timeout row for the skipped sequence {skipped_seq}"
    );
}

/// R2 positive control for the fence-cleared route: a hole abandoned by a
/// rolled-back transaction is skipped by snapshot fencing
/// (`snapshot_fencing: true`) WITHOUT recording a gap-timeout row, and the
/// checkpoint still advances past it.
///
/// `#[ignore]`d deliberately (spec 0027 §7.3). Fence clearing needs
/// `snapshot.xmin >= fence_xmax`, and `xmin` is instance-wide, so any
/// transaction open anywhere on the server (including sibling tests in this
/// suite, which hold transactions for seconds) pins it and makes this flake.
/// `gap_timeout` is 30s here so a pass cannot be the backstop in disguise. The
/// no-stall guarantee is already covered with zero load sensitivity by
/// `test_backstop_hole_still_advances_checkpoint`, and the fence route by the
/// existing `test_rolled_back_gap_fence_clears_without_record`, so this is
/// redundant assurance that ships ignored. Run it on a quiet instance:
/// `cargo test -p epoch_pg -- --ignored test_fence_cleared_hole_still_advances_checkpoint`.
#[tokio::test]
#[serial]
#[ignore = "instance-wide xmin makes fence clearing flaky under load; run on a quiet instance"]
async fn test_fence_cleared_hole_still_advances_checkpoint() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

    let table = isolated_events_table(&pool).await;

    // Fencing on, gap_timeout long enough that a pass within the poll window
    // below cannot be the backstop firing.
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        snapshot_fencing: true,
        gap_timeout: GapDuration::from_secs(30),
        events_table: table.clone(),
        ..Default::default()
    };

    let channel_name = format!("test_fence_cleared_{}", Uuid::new_v4().simple());
    let event_bus = PgEventBus::<TestEventData>::with_config(pool.clone(), channel_name, config);
    event_bus
        .setup_trigger()
        .await
        .expect("Failed to setup trigger");
    event_bus
        .start_listener()
        .await
        .expect("Failed to start listener");

    let subscriber_id = format!("projection:fence-cleared:{}", Uuid::new_v4());
    let projection = TestProjection::with_subscriber_id(subscriber_id.clone());
    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // create_sequence_gap rolls its transaction back before returning, so the
    // hole's writer has provably ended and the fence can prove it will never
    // fill.
    let stream_id = Uuid::new_v4();
    let (_before_id, skipped_seq, _after_id) = create_sequence_gap(&pool, &table, stream_id).await;
    let seq_above = skipped_seq + 1;

    // Poll up to ~15s, well under the 30s gap_timeout so a pass is the fence,
    // not the backstop. The two-tick floor (fence_xmax is None on first
    // observation, backfilled next batch) means this needs several seconds even
    // on a quiet instance.
    let mut checkpoint = None;
    for _ in 0..60 {
        checkpoint = event_bus
            .get_checkpoint(&subscriber_id)
            .await
            .expect("get checkpoint");
        if matches!(checkpoint, Some(s) if s >= seq_above) {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
    }

    let gap_records = event_bus
        .list_gap_timeouts(Some(&subscriber_id), false, 0, 50)
        .await
        .expect("Failed to list gap timeouts");

    event_bus.shutdown().await.expect("shutdown");
    drop_isolated_events_table(&pool, &table).await;

    assert!(
        matches!(checkpoint, Some(s) if s >= seq_above),
        "fencing must advance the checkpoint ({checkpoint:?}) past the cleared \
         hole to at least {seq_above}"
    );
    assert!(
        gap_records.is_empty(),
        "fence clearing must NOT record a gap-timeout row; found: {gap_records:?}"
    );
}

// ============================================================================
// Spec 0027 / CLOUD-232: live-path contiguous checkpoint regression tests
//
// These pin the §3.3 invariant's "never leads" direction on the LIVE listener
// path: after a flush from any site, the persisted checkpoint MUST NOT lead
// `state.contiguous_checkpoint`. Each holds a hole open with an uncommitted
// transaction, commits events above it, and proves the persisted checkpoint
// stays at the last contiguous sequence below the hole.
//
// All run on an isolated events table with its OWN sequence (§7.1) so the hole
// is burned on a private sequence and the below-hole sequence is deterministic,
// letting the assertions use EXACT equality rather than `< seq_hole`. A
// `< seq_hole` near-miss passes vacuously when nothing was written (e.g. a
// sibling binary's TRUNCATE wiped the row); committing a known below-hole event
// first and asserting equality against it converts that silent pass into a loud
// failure, proves the write path is alive, and proves the fix did not move the
// checkpoint backwards (§7.2, a review gate for this spec).
//
// Ordering discipline (inherited from CLOUD-226): observe the checkpoint, then
// release the held transaction and shut down, THEN assert. An `assert!` that
// unwinds with the hole's transaction still open is the shape that once wedged
// this project for hours.
//
// Reverting Phase 2 (the fix) makes tests 1-3 fail: the pre-fix live path wrote
// the batch's max-seen sequence, which sits above the held hole (acceptance
// §9.2, verified by stashing).
// ============================================================================

/// Reads the full persisted checkpoint row (`last_global_sequence`,
/// `last_event_id`) for `subscriber_id` on `table`'s bus. `get_checkpoint`
/// returns only the sequence; the R5 pairing assertion needs the id too. The
/// bus_name is the events table (spec §7.1), so an isolated table's checkpoints
/// are keyed by that table.
async fn read_persisted_checkpoint(
    pool: &PgPool,
    table: &str,
    subscriber_id: &str,
) -> Option<(i64, Option<Uuid>)> {
    sqlx::query_as(
        r#"SELECT last_global_sequence, last_event_id
           FROM epoch_event_bus_checkpoints
           WHERE bus_name = $1 AND subscriber_id = $2"#,
    )
    .bind(table)
    .bind(subscriber_id)
    .fetch_optional(pool)
    .await
    .expect("read persisted checkpoint row")
}

/// R1 + R5 PRIMARY regression test. Under the default `Synchronous` mode (stated
/// explicitly below because that is the whole point: on the production default
/// only the shutdown flush can leak), the shutdown flush
/// (`flush_all_pending_checkpoints`) must NOT publish a sequence above a hole
/// still held by an open transaction, and the persisted `last_event_id` must
/// stay paired to the below-hole event.
///
/// Pre-fix this reads `Some(seq_above2)` with a mismatched id; post-fix it reads
/// exactly the below-hole sequence and its id. This is the only DB-level R5
/// check in the plan, and the eager-seed path is precisely where the pairing can
/// break.
#[tokio::test]
#[serial]
async fn test_live_shutdown_does_not_publish_above_held_hole() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

    let table = isolated_events_table(&pool).await;

    // Default checkpoint_mode is Synchronous: on this production default the
    // steady-state checkpoint already holds at a hole (existing
    // test_in_flight_transaction_gap_is_held), so only the shutdown flush can
    // leak, and that is exactly what this test exercises. gap_timeout is long so
    // the backstop cannot skip the hole inside the test window.
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        gap_timeout: GapDuration::from_secs(30),
        events_table: table.clone(),
        ..Default::default()
    };
    assert!(
        matches!(
            config.checkpoint_mode,
            epoch_pg::event_bus::CheckpointMode::Synchronous
        ),
        "this test relies on the default Synchronous mode"
    );

    let channel_name = format!("test_live_shutdown_{}", Uuid::new_v4().simple());
    let event_bus = PgEventBus::<TestEventData>::with_config(pool.clone(), channel_name, config);
    event_bus
        .setup_trigger()
        .await
        .expect("Failed to setup trigger");
    event_bus
        .start_listener()
        .await
        .expect("Failed to start listener");

    let subscriber_id = format!("projection:live-shutdown:{}", Uuid::new_v4());
    let projection = TestProjection::with_subscriber_id(subscriber_id.clone());
    let store = projection.get_state_store().clone();
    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe");
    tokio::time::sleep(GapDuration::from_millis(100)).await;

    // A committed event below the hole, on its own stream (the hole and the
    // above-hole events share a stream and would collide on the
    // (stream_id, stream_version) unique constraint otherwise). Poll until it
    // is the persisted contiguous checkpoint: that both proves the write path
    // is alive and gives the exact value the leak must not exceed.
    let below_stream = Uuid::new_v4();
    let (below_id, seq_below) =
        insert_committed_event(&pool, &table, below_stream, 1, "below_hole").await;
    let mut established = false;
    for _ in 0..40 {
        if event_bus
            .get_checkpoint(&subscriber_id)
            .await
            .expect("get checkpoint")
            == Some(seq_below as u64)
        {
            established = true;
            break;
        }
        tokio::time::sleep(GapDuration::from_millis(100)).await;
    }
    assert!(
        established,
        "the below-hole checkpoint ({seq_below}) must be persisted before the hole is opened"
    );

    // Hold a hole open on the private sequence, then commit two events above it.
    let hole_stream = Uuid::new_v4();
    let mut tx = pool.begin().await.expect("begin hole tx");
    let (_hole_id, seq_hole) = claim_hole_uncommitted(&mut tx, &table, hole_stream).await;
    assert!(
        seq_hole > seq_below,
        "the hole must sit above the below-hole event"
    );
    let (above1_id, seq_above1) =
        insert_committed_event(&pool, &table, hole_stream, 2, "above1").await;
    let (above2_id, seq_above2) =
        insert_committed_event(&pool, &table, hole_stream, 3, "above2").await;
    assert!(
        seq_above1 > seq_hole && seq_above2 > seq_above1,
        "the above-hole events must sit above the hole"
    );

    // Bounded poll proving the above-hole events were delivered. That delivery
    // is what populates pending_checkpoint with max-seen pre-fix; without it the
    // regression assertion would pass vacuously.
    let mut delivered = false;
    for _ in 0..40 {
        if let Some(state) = store.get_state(hole_stream).await.unwrap() {
            let seen: Vec<Uuid> = state.0.iter().map(|e| e.id).collect();
            if seen.contains(&above1_id) && seen.contains(&above2_id) {
                delivered = true;
                break;
            }
        }
        tokio::time::sleep(GapDuration::from_millis(100)).await;
    }
    assert!(
        delivered,
        "both above-hole events must be delivered (proves pending_checkpoint saw max-seen)"
    );

    // Shut down with the hole's transaction STILL OPEN: the shutdown flush is
    // the only Synchronous-mode leak site. Observe, then release, then assert.
    event_bus.shutdown().await.expect("shutdown");
    let persisted = read_persisted_checkpoint(&pool, &table, &subscriber_id).await;

    tx.rollback().await.expect("rollback hole tx");
    drop_isolated_events_table(&pool, &table).await;

    let (persisted_seq, persisted_id) =
        persisted.expect("a checkpoint row must exist after the below-hole flush");
    assert_eq!(
        persisted_seq, seq_below,
        "shutdown must persist exactly the below-hole sequence ({seq_below}), not a value \
         above the held hole (pre-fix this was {seq_above2})"
    );
    assert_eq!(
        persisted_id,
        Some(below_id),
        "the persisted last_event_id must stay paired to the below-hole event (R5)"
    );
}

/// R1 regression test for the `Batched` `max_delay_ms` timer flush
/// (`flush_expired_checkpoints`). With `batch_size` high so only the delay can
/// fire, the periodic flush must NOT publish above a held hole.
///
/// Pre-fix the timer tick publishes the batch's max-seen sequence (above the
/// hole); post-fix it leaves the persisted checkpoint at the below-hole value.
#[tokio::test]
#[serial]
async fn test_live_batched_flush_does_not_publish_above_held_hole() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

    let table = isolated_events_table(&pool).await;

    // batch_size high so the size trigger never fires; only the max_delay timer
    // can flush. gap_timeout long so the backstop cannot skip the hole.
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        checkpoint_mode: epoch_pg::event_bus::CheckpointMode::Batched {
            batch_size: 1000,
            max_delay_ms: 300,
        },
        gap_timeout: GapDuration::from_secs(30),
        events_table: table.clone(),
        ..Default::default()
    };

    let channel_name = format!("test_live_batched_{}", Uuid::new_v4().simple());
    let event_bus = PgEventBus::<TestEventData>::with_config(pool.clone(), channel_name, config);
    event_bus
        .setup_trigger()
        .await
        .expect("Failed to setup trigger");
    event_bus
        .start_listener()
        .await
        .expect("Failed to start listener");

    let subscriber_id = format!("projection:live-batched:{}", Uuid::new_v4());
    let projection = TestProjection::with_subscriber_id(subscriber_id.clone());
    let store = projection.get_state_store().clone();
    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe");
    tokio::time::sleep(GapDuration::from_millis(100)).await;

    // Below-hole event; poll until the max_delay timer persists it.
    let below_stream = Uuid::new_v4();
    let (_below_id, seq_below) =
        insert_committed_event(&pool, &table, below_stream, 1, "below_hole").await;
    let mut established = false;
    for _ in 0..40 {
        if event_bus
            .get_checkpoint(&subscriber_id)
            .await
            .expect("get checkpoint")
            == Some(seq_below as u64)
        {
            established = true;
            break;
        }
        tokio::time::sleep(GapDuration::from_millis(100)).await;
    }
    assert!(
        established,
        "the below-hole checkpoint ({seq_below}) must be persisted before the hole is opened"
    );

    // Hold a hole, commit two events above it.
    let hole_stream = Uuid::new_v4();
    let mut tx = pool.begin().await.expect("begin hole tx");
    let (_hole_id, seq_hole) = claim_hole_uncommitted(&mut tx, &table, hole_stream).await;
    assert!(
        seq_hole > seq_below,
        "the hole must sit above the below-hole event"
    );
    let (above1_id, seq_above1) =
        insert_committed_event(&pool, &table, hole_stream, 2, "above1").await;
    let (above2_id, seq_above2) =
        insert_committed_event(&pool, &table, hole_stream, 3, "above2").await;
    assert!(
        seq_above1 > seq_hole && seq_above2 > seq_above1,
        "the above-hole events must sit above the hole"
    );

    // Verify delivery of the above-hole events BEFORE the fixed sleep, so the
    // sleep only has to cover the flush tick, not delivery latency too.
    let mut delivered = false;
    for _ in 0..40 {
        if let Some(state) = store.get_state(hole_stream).await.unwrap() {
            let seen: Vec<Uuid> = state.0.iter().map(|e| e.id).collect();
            if seen.contains(&above1_id) && seen.contains(&above2_id) {
                delivered = true;
                break;
            }
        }
        tokio::time::sleep(GapDuration::from_millis(100)).await;
    }
    assert!(
        delivered,
        "both above-hole events must be delivered before the flush tick is awaited"
    );

    // ONE unavoidable fixed sleep. "Checkpoint has not advanced" is both the
    // expected post-condition AND what a stalled listener looks like, so there
    // is no positive edge to poll for: a poll loop here can never fail and would
    // make the test vacuous. DO NOT convert this to a poll loop. Sized at
    // >= 2x the hard-coded 1s flush_interval so the max_delay (300ms) timer tick
    // is guaranteed to have fired over the held hole. Delivery is already
    // verified above, so this only covers the flush tick.
    tokio::time::sleep(GapDuration::from_millis(2500)).await;

    let checkpoint = event_bus
        .get_checkpoint(&subscriber_id)
        .await
        .expect("get checkpoint");

    tx.rollback().await.expect("rollback hole tx");
    event_bus.shutdown().await.expect("shutdown");
    drop_isolated_events_table(&pool, &table).await;

    assert_eq!(
        checkpoint,
        Some(seq_below as u64),
        "the Batched max_delay timer must leave the checkpoint at the below-hole sequence \
         ({seq_below}), not publish above the held hole (pre-fix this was {seq_above2})"
    );
}

/// R4 coverage gap closed after 0027 review round 1: ahead-of-gap events must
/// still count toward the `Batched` `batch_size` threshold (`record_processed`
/// feeds the counter), but a threshold crossing while a hole is held must still
/// be unable to publish above the hole (the `is_publishable` predicate blocks
/// it). The sibling test above deliberately sets `batch_size: 1000` to isolate
/// the `max_delay` timer path; this test isolates the `batch_size` path by
/// setting `max_delay_ms` long enough that only a `batch_size` crossing can
/// fire.
///
/// Every processed row bumps the shared counter, including the below-hole
/// event itself (`record_processed` runs at both the success and deser-skip
/// per-row sites, regardless of contiguity), so `batch_size: 3` is chosen
/// deliberately: the below-hole event alone contributes 1, which is not close
/// to the threshold, so crossing it requires **both** above-hole events to be
/// processed (1 + 1 + 1 = 3). A smaller `batch_size: 2` would let the
/// below-hole event's own count plus a single ahead event cross the threshold,
/// which would not cleanly demonstrate that ahead-of-gap events themselves are
/// being counted.
///
/// This closes a real gap: without `record_processed` feeding the counter,
/// `events_since_checkpoint` would freeze and this crossing would never fire at
/// all (the bug §3.1 describes for the naive per-event `update()` deletion);
/// without the predicate, the crossing would publish `seq_above2`. Reverting
/// either `record_processed`'s counter bump or the `is_publishable` guard in
/// `try_flush_pending_checkpoint`'s `take_if` closure makes this test fail —
/// reasoned through the code below, not executed against a reverted tree,
/// since only one worktree may write here at a time. Reverting the counter
/// bump: the pending would stay below `batch_size` since events accrue only
/// via `record_processed`, `should_flush_checkpoint` would never return `true`
/// from the `batch_size` arm, and the checkpoint poll below would time out at
/// `None`/the below-hole value never reached by this path, since nothing would
/// flush without the size trigger. Reverting the predicate: the `take_if`
/// would flush the pending unconditionally once `events_since_checkpoint >=
/// batch_size`, publishing `seq_above2` instead of stalling at `seq_below`, and
/// the final `assert_eq!` below would observe `seq_above2` — the same failure
/// mode the sibling `max_delay` test pins.
#[tokio::test]
#[serial]
async fn test_live_batched_flush_counts_ahead_events_but_does_not_publish_above_held_hole() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

    let table = isolated_events_table(&pool).await;

    // batch_size small enough that the below-hole event plus both ahead-of-gap
    // events cross it (see the deliberate batch_size: 3 reasoning above);
    // max_delay long so only the batch_size trigger can fire. gap_timeout long
    // so the backstop cannot skip the hole inside the test window.
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        checkpoint_mode: epoch_pg::event_bus::CheckpointMode::Batched {
            batch_size: 3,
            max_delay_ms: 60000,
        },
        gap_timeout: GapDuration::from_secs(30),
        events_table: table.clone(),
        ..Default::default()
    };

    let channel_name = format!("test_live_batched_bs_{}", Uuid::new_v4().simple());
    let event_bus = PgEventBus::<TestEventData>::with_config(pool.clone(), channel_name, config);
    event_bus
        .setup_trigger()
        .await
        .expect("Failed to setup trigger");
    event_bus
        .start_listener()
        .await
        .expect("Failed to start listener");

    let subscriber_id = format!("projection:live-batched-bs:{}", Uuid::new_v4());
    let projection = TestProjection::with_subscriber_id(subscriber_id.clone());
    let store = projection.get_state_store().clone();
    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe");
    tokio::time::sleep(GapDuration::from_millis(100)).await;

    // Below-hole event. Deliberately NOT established via a poll before opening
    // the hole (unlike the sibling tests): this event's own processing
    // contributes only 1 toward the counter, well short of batch_size: 3, so no
    // flush is expected here and there is nothing to poll for yet.
    let below_stream = Uuid::new_v4();
    let (below_id, seq_below) =
        insert_committed_event(&pool, &table, below_stream, 1, "below_hole").await;

    // Hold a hole, then commit two above-hole events. Combined with the
    // below-hole event's own count, this reaches batch_size: 3 (1 + 1 + 1)
    // while the hole is still open.
    let hole_stream = Uuid::new_v4();
    let mut tx = pool.begin().await.expect("begin hole tx");
    let (_hole_id, seq_hole) = claim_hole_uncommitted(&mut tx, &table, hole_stream).await;
    assert!(
        seq_hole > seq_below,
        "the hole must sit above the below-hole event"
    );
    let (above1_id, seq_above1) =
        insert_committed_event(&pool, &table, hole_stream, 2, "above1").await;
    let (above2_id, seq_above2) =
        insert_committed_event(&pool, &table, hole_stream, 3, "above2").await;
    assert!(
        seq_above1 > seq_hole && seq_above2 > seq_above1,
        "the above-hole events must sit above the hole"
    );

    // Bounded poll proving both above-hole events were delivered (and so
    // counted via record_processed), before asserting on the checkpoint.
    let mut delivered = false;
    for _ in 0..40 {
        if let Some(state) = store.get_state(hole_stream).await.unwrap() {
            let seen: Vec<Uuid> = state.0.iter().map(|e| e.id).collect();
            if seen.contains(&above1_id) && seen.contains(&above2_id) {
                delivered = true;
                break;
            }
        }
        tokio::time::sleep(GapDuration::from_millis(100)).await;
    }
    assert!(
        delivered,
        "both above-hole events must be delivered, proving they were counted toward batch_size"
    );

    // The batch_size threshold has now been crossed (1 below-hole event + 2
    // ahead-of-gap events = 3 >= batch_size: 3). Poll for the checkpoint to
    // reach exactly the below-hole sequence: reaching it proves the crossing
    // triggered a flush attempt (record_processed fed the counter for the
    // ahead-of-gap events); never exceeding it proves the predicate still
    // blocked publishing the ahead-of-gap max.
    let mut checkpoint = None;
    for _ in 0..40 {
        checkpoint = event_bus
            .get_checkpoint(&subscriber_id)
            .await
            .expect("get checkpoint");
        if checkpoint == Some(seq_below as u64) {
            break;
        }
        tokio::time::sleep(GapDuration::from_millis(100)).await;
    }

    tx.rollback().await.expect("rollback hole tx");
    event_bus.shutdown().await.expect("shutdown");
    let persisted = read_persisted_checkpoint(&pool, &table, &subscriber_id).await;
    drop_isolated_events_table(&pool, &table).await;

    assert_eq!(
        checkpoint,
        Some(seq_below as u64),
        "the batch_size crossing over ahead-of-gap events must persist exactly the below-hole \
         sequence ({seq_below}), proving record_processed fed the counter without publishing \
         above the held hole (a version that never counted ahead-of-gap events would time out \
         at None here instead; a version without the publishable predicate would show \
         {seq_above2})"
    );
    let (persisted_seq, persisted_id) =
        persisted.expect("a checkpoint row must exist after the batch_size-triggered flush");
    assert_eq!(
        persisted_seq, seq_below,
        "the persisted checkpoint must never exceed the below-hole sequence ({seq_below})"
    );
    assert_eq!(
        persisted_id,
        Some(below_id),
        "the persisted last_event_id must stay paired to the below-hole event"
    );
}

/// R1 regression test for the SECOND unconditional write site: the
/// deserialization-skip branch. No existing test covers this branch above a
/// hole. The above-hole rows are raw inserts whose `data` is VALID JSON that is
/// not a known event variant (`{"NoSuchVariant":{}}`), so `serde_json::from_value`
/// fails and the branch `continue`s after a WARN. Genuine garbage bytes would be
/// rejected by Postgres at INSERT (the column is `jsonb`) and never reach
/// deserialization.
///
/// Mode/trigger stated explicitly (the shared preamble does not apply here):
/// default `Synchronous` plus `shutdown()`. The delivery poll is IMPOSSIBLE here
/// because the deser branch `continue`s, so those events are never delivered;
/// the WARN assertion (`captured_logs_contain_since`) is its SUBSTITUTE signal,
/// proving the deser-skip branch ran and populated pending_checkpoint.
#[tokio::test]
#[serial]
async fn test_live_deser_skip_above_hole_does_not_publish_above_hole() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

    let table = isolated_events_table(&pool).await;

    // Default Synchronous mode; shutdown() is the flush trigger.
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        gap_timeout: GapDuration::from_secs(30),
        events_table: table.clone(),
        ..Default::default()
    };
    assert!(
        matches!(
            config.checkpoint_mode,
            epoch_pg::event_bus::CheckpointMode::Synchronous
        ),
        "this test relies on the default Synchronous mode"
    );

    let channel_name = format!("test_live_deser_{}", Uuid::new_v4().simple());
    let event_bus = PgEventBus::<TestEventData>::with_config(pool.clone(), channel_name, config);
    event_bus
        .setup_trigger()
        .await
        .expect("Failed to setup trigger");
    event_bus
        .start_listener()
        .await
        .expect("Failed to start listener");

    let subscriber_id = format!("projection:live-deser:{}", Uuid::new_v4());
    let projection = TestProjection::with_subscriber_id(subscriber_id.clone());
    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe");
    tokio::time::sleep(GapDuration::from_millis(100)).await;

    // Below-hole event; poll until it is the persisted checkpoint.
    let below_stream = Uuid::new_v4();
    let (below_id, seq_below) =
        insert_committed_event(&pool, &table, below_stream, 1, "below_hole").await;
    let mut established = false;
    for _ in 0..40 {
        if event_bus
            .get_checkpoint(&subscriber_id)
            .await
            .expect("get checkpoint")
            == Some(seq_below as u64)
        {
            established = true;
            break;
        }
        tokio::time::sleep(GapDuration::from_millis(100)).await;
    }
    assert!(
        established,
        "the below-hole checkpoint ({seq_below}) must be persisted before the hole is opened"
    );

    // Hold a hole, then commit two above-hole rows whose data is valid JSON but
    // not a known TestEventData variant. Snapshot the log length first so the
    // WARN assertion only scans lines emitted from here on.
    let hole_stream = Uuid::new_v4();
    let mut tx = pool.begin().await.expect("begin hole tx");
    let (_hole_id, seq_hole) = claim_hole_uncommitted(&mut tx, &table, hole_stream).await;
    assert!(
        seq_hole > seq_below,
        "the hole must sit above the below-hole event"
    );

    let log_start = common::captured_logs_len();
    let bad_data = serde_json::json!({ "NoSuchVariant": {} });
    let mut seq_above_last = 0i64;
    for version in 2..=3i64 {
        let seq: i64 = sqlx::query_scalar(&format!(
            r#"INSERT INTO {table} (id, stream_id, stream_version, event_type, data, created_at)
               VALUES ($1, $2, $3, 'MyEvent', $4, NOW())
               RETURNING global_sequence"#
        ))
        .bind(Uuid::new_v4())
        .bind(hole_stream)
        .bind(version)
        .bind(&bad_data)
        .fetch_one(&pool)
        .await
        .expect("insert undeserializable above-hole event");
        seq_above_last = seq;
    }
    assert!(
        seq_above_last > seq_hole,
        "the above-hole rows must sit above the hole"
    );

    // Substitute for the impossible delivery poll: wait until the deser-skip
    // WARN fires, proving the branch ran and populated pending_checkpoint.
    //
    // "failed to deserialize" alone is not scoped to this subscriber: it also
    // appears in the catch-up, buffer-processing and read_all_events_since skip
    // sites, so a leftover listener task from an earlier test in this binary
    // could satisfy it for the wrong reason. The live-path WARN formats
    // `for '{subscriber_id}': failed to deserialize`, so require that exact
    // adjacency rather than the bare message substring.
    let needle = format!("for '{subscriber_id}': failed to deserialize");
    let mut warned = false;
    for _ in 0..40 {
        if common::captured_logs_contain_since(log_start, &needle) {
            warned = true;
            break;
        }
        tokio::time::sleep(GapDuration::from_millis(100)).await;
    }
    assert!(
        warned,
        "the deserialization-skip WARN for this subscriber must fire, proving the branch above the hole ran"
    );

    // Shut down with the hole still held; observe, then release, then assert.
    event_bus.shutdown().await.expect("shutdown");
    let persisted = read_persisted_checkpoint(&pool, &table, &subscriber_id).await;

    tx.rollback().await.expect("rollback hole tx");
    drop_isolated_events_table(&pool, &table).await;

    let (persisted_seq, persisted_id) =
        persisted.expect("a checkpoint row must exist after the below-hole flush");
    assert_eq!(
        persisted_seq, seq_below,
        "the deser-skip branch must not publish above the held hole; the checkpoint must \
         stay at the below-hole sequence ({seq_below}, pre-fix this was {seq_above_last})"
    );
    assert_eq!(
        persisted_id,
        Some(below_id),
        "the persisted last_event_id must stay paired to the below-hole event"
    );
}

/// R6 regression guard: a `ReplayAlways` subscriber must write NO checkpoint from
/// the live listener path. `process_subscriber_for_batch` nulls its
/// `pending_checkpoint` after the contiguous branch, and the new unconditional
/// flush sits inside a `!replay_always` guard, so live delivery must leave the
/// checkpoints table empty for it.
#[tokio::test]
#[serial]
async fn test_live_replay_always_writes_no_checkpoint() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

    let table = isolated_events_table(&pool).await;

    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        events_table: table.clone(),
        ..Default::default()
    };

    let channel_name = format!("test_live_replay_always_{}", Uuid::new_v4().simple());
    let event_bus = PgEventBus::<TestEventData>::with_config(pool.clone(), channel_name, config);
    event_bus
        .setup_trigger()
        .await
        .expect("Failed to setup trigger");
    event_bus
        .start_listener()
        .await
        .expect("Failed to start listener");

    let subscriber_id = format!("projection:live-replay-always:{}", Uuid::new_v4());
    let projection = TestProjection::replay_always(subscriber_id.clone());
    let store = projection.get_state_store().clone();
    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe");
    tokio::time::sleep(GapDuration::from_millis(100)).await;

    // Commit events live (after subscribe) so they travel the live path, and
    // poll until delivered.
    let stream_id = Uuid::new_v4();
    let mut ids = Vec::new();
    for version in 1..=3i64 {
        let (id, _seq) =
            insert_committed_event(&pool, &table, stream_id, version, &format!("e{version}")).await;
        ids.push(id);
    }
    let mut delivered = false;
    for _ in 0..40 {
        if let Some(state) = store.get_state(stream_id).await.unwrap() {
            let seen: Vec<Uuid> = state.0.iter().map(|e| e.id).collect();
            if ids.iter().all(|id| seen.contains(id)) {
                delivered = true;
                break;
            }
        }
        tokio::time::sleep(GapDuration::from_millis(100)).await;
    }
    assert!(
        delivered,
        "the ReplayAlways subscriber must have received the live events before assertion"
    );

    event_bus.shutdown().await.expect("shutdown");
    let checkpoint = event_bus
        .get_checkpoint(&subscriber_id)
        .await
        .expect("get checkpoint");
    drop_isolated_events_table(&pool, &table).await;

    assert_eq!(
        checkpoint, None,
        "a ReplayAlways subscriber must write no checkpoint from the live path (R6)"
    );
}

// ============================================================================
// Spec 0028 (CLOUD-216) — fail-closed live-path delivery, observability, and
// panic containment (phase P2).
//
// Every test here runs on an `isolated_events_table` under `#[serial]`, per the
// standing isolation rule: a fail-closed subscriber starting at checkpoint 0
// would otherwise wedge on any malformed row a sibling binary left on the
// shared `epoch_events` table.
// ============================================================================

use epoch_pg::event_bus::{DlqCallback, DlqInsertionInfo, HaltCallback, HaltInfo, HaltReason};
use std::collections::HashMap as StdHashMap;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Inserts a committed row whose `data` cannot be deserialized as
/// `TestEventData`, returning its id and assigned `global_sequence`.
async fn insert_corrupt_event(
    pool: &PgPool,
    table: &str,
    stream_id: Uuid,
    version: i64,
) -> (Uuid, i64) {
    let id = Uuid::new_v4();
    let data = serde_json::json!({ "garbage": true, "not_a_variant": 123 });
    let seq: i64 = sqlx::query_scalar(&format!(
        r#"INSERT INTO {table} (id, stream_id, stream_version, event_type, data, created_at)
           VALUES ($1, $2, $3, 'MyEvent', $4, NOW())
           RETURNING global_sequence"#
    ))
    .bind(id)
    .bind(stream_id)
    .bind(version)
    .bind(&data)
    .fetch_one(pool)
    .await
    .expect("insert corrupt event");
    (id, seq)
}

/// Raw `SELECT` against `epoch_event_bus_dlq` for one subscriber. There is no
/// bus-level DLQ-read API (`list_dlq_entries` does not exist), so tests read the
/// table directly. Returns `(event_id, global_sequence, error_message)` rows.
async fn read_dlq_rows(pool: &PgPool, subscriber_id: &str) -> Vec<(Uuid, i64, String)> {
    sqlx::query_as::<_, (Uuid, i64, String)>(
        r#"SELECT event_id, global_sequence, error_message
           FROM epoch_event_bus_dlq
           WHERE subscriber_id = $1
           ORDER BY global_sequence ASC"#,
    )
    .bind(subscriber_id)
    .fetch_all(pool)
    .await
    .expect("read dlq rows")
}

/// Rewrites a row's `data` payload so a previously-undeserializable event now
/// deserializes (drives the self-healing recovery path, spec 0028 §3.4).
async fn fix_event_payload(pool: &PgPool, table: &str, global_sequence: i64, value: &str) {
    let data = serde_json::to_value(Some(TestEventData::TestEvent {
        value: value.to_string(),
    }))
    .unwrap();
    sqlx::query(&format!(
        "UPDATE {table} SET data = $1 WHERE global_sequence = $2"
    ))
    .bind(&data)
    .bind(global_sequence)
    .execute(pool)
    .await
    .expect("fix event payload");
}

/// Builds and starts a `PgEventBus` on `table` with `config`, using a fresh
/// per-test NOTIFY channel. The caller is responsible for setting
/// `config.events_table` to `table`.
async fn start_isolated_bus(
    pool: &PgPool,
    config: epoch_pg::event_bus::ReliableDeliveryConfig,
) -> PgEventBus<TestEventData> {
    let channel_name = format!("test_fc_{}", Uuid::new_v4().simple());
    let event_bus = PgEventBus::<TestEventData>::with_config(pool.clone(), channel_name, config);
    event_bus
        .setup_trigger()
        .await
        .expect("Failed to setup trigger");
    event_bus
        .start_listener()
        .await
        .expect("Failed to start listener");
    event_bus
}

/// Captures every `HaltInfo` the bus fires, for assertions on halt entry.
struct CapturingHaltCallback {
    halts: Arc<StdMutex<Vec<HaltInfo>>>,
}

#[async_trait]
impl HaltCallback for CapturingHaltCallback {
    async fn on_halt(&self, info: HaltInfo) {
        self.halts.lock().unwrap().push(info);
    }
}

/// Captures every `DlqInsertionInfo` the bus fires (observer-exhaustion path).
struct CapturingDlqCallback {
    inserts: Arc<StdMutex<Vec<DlqInsertionInfo>>>,
}

#[async_trait]
impl DlqCallback for CapturingDlqCallback {
    async fn on_dlq_insertion(&self, info: DlqInsertionInfo) {
        self.inserts.lock().unwrap().push(info);
    }
}

/// An `EventObserver` whose `on_event` fails while `healthy` is `false` and
/// succeeds once flipped `true`, counting invocations per sequence so a test can
/// poll the retry/re-attempt cadence (spec 0028 R4).
struct CountingFailingObserver {
    subscriber_id: String,
    failure_mode: FailureMode,
    healthy: Arc<AtomicBool>,
    invocations: Arc<StdMutex<StdHashMap<u64, u32>>>,
    applied: Arc<StdMutex<Vec<u64>>>,
}

impl epoch_core::SubscriberId for CountingFailingObserver {
    fn subscriber_id(&self) -> &str {
        &self.subscriber_id
    }
}

#[async_trait]
impl EventObserver<TestEventData> for CountingFailingObserver {
    async fn on_event(
        &self,
        event: Arc<Event<TestEventData>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let seq = event.global_sequence.unwrap_or(0);
        *self.invocations.lock().unwrap().entry(seq).or_insert(0) += 1;
        if self.healthy.load(Ordering::SeqCst) {
            self.applied.lock().unwrap().push(seq);
            Ok(())
        } else {
            Err(format!("counting-failing observer down for seq {seq}").into())
        }
    }

    fn failure_mode(&self) -> FailureMode {
        self.failure_mode
    }
}

/// An `EventObserver` whose `on_event` always panics with a deterministic
/// message (spec 0028 §3.6 panic containment).
struct PanickingObserver {
    subscriber_id: String,
    failure_mode: FailureMode,
    invocations: Arc<AtomicU32>,
}

impl epoch_core::SubscriberId for PanickingObserver {
    fn subscriber_id(&self) -> &str {
        &self.subscriber_id
    }
}

#[async_trait]
impl EventObserver<TestEventData> for PanickingObserver {
    async fn on_event(
        &self,
        _event: Arc<Event<TestEventData>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        panic!("panicking observer: deterministic boom");
    }

    fn failure_mode(&self) -> FailureMode {
        self.failure_mode
    }
}

fn seq_invocations(invocations: &Arc<StdMutex<StdHashMap<u64, u32>>>, seq: u64) -> u32 {
    invocations.lock().unwrap().get(&seq).copied().unwrap_or(0)
}

/// Polls until the DLQ has at least one row for `subscriber_id`, returning them.
async fn poll_dlq_rows(pool: &PgPool, subscriber_id: &str) -> Vec<(Uuid, i64, String)> {
    for _ in 0..80 {
        let rows = read_dlq_rows(pool, subscriber_id).await;
        if !rows.is_empty() {
            return rows;
        }
        tokio::time::sleep(GapDuration::from_millis(50)).await;
    }
    read_dlq_rows(pool, subscriber_id).await
}

/// Polls until `subscriber_id`'s persisted checkpoint equals `expected`.
async fn poll_checkpoint_eq(
    event_bus: &PgEventBus<TestEventData>,
    subscriber_id: &str,
    expected: Option<u64>,
) -> bool {
    for _ in 0..80 {
        if event_bus
            .get_checkpoint(subscriber_id)
            .await
            .expect("get checkpoint")
            == expected
        {
            return true;
        }
        tokio::time::sleep(GapDuration::from_millis(100)).await;
    }
    false
}

/// T1 (R2, R7, R8): a fail-closed live deserialize failure holds the contiguous
/// checkpoint below the bad sequence, writes a DLQ row, fires `on_halt`, blocks
/// readiness, and leaves a healthy fail-open peer unaffected.
#[tokio::test]
#[serial]
async fn test_live_deser_halt_holds_checkpoint_fail_closed() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    let table = isolated_events_table(&pool).await;

    let halts = Arc::new(StdMutex::new(Vec::new()));
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        gap_timeout: GapDuration::from_secs(30),
        events_table: table.clone(),
        on_halt: Some(Arc::new(CapturingHaltCallback {
            halts: halts.clone(),
        })),
        ..Default::default()
    };
    let event_bus = start_isolated_bus(&pool, config).await;

    let fc_id = format!("projection:fc-deser:{}", Uuid::new_v4());
    let fc = TestProjection::with_subscriber_id(fc_id.clone()).fail_closed();
    let fc_store = fc.get_state_store().clone();
    event_bus
        .subscribe(ProjectionHandler::new(fc))
        .await
        .expect("subscribe fc");

    let peer_id = format!("projection:peer:{}", Uuid::new_v4());
    let peer = TestProjection::with_subscriber_id(peer_id.clone());
    let peer_store = peer.get_state_store().clone();
    event_bus
        .subscribe(ProjectionHandler::new(peer))
        .await
        .expect("subscribe peer");

    tokio::time::sleep(GapDuration::from_millis(100)).await;

    let stream = Uuid::new_v4();
    let (valid1_id, seq1) = insert_committed_event(&pool, &table, stream, 1, "v1").await;
    let (corrupt_id, seq2) = insert_corrupt_event(&pool, &table, stream, 2).await;
    let (valid3_id, seq3) = insert_committed_event(&pool, &table, stream, 3, "v3").await;

    // The healthy peer skips the corrupt row and advances to the tail.
    assert!(
        poll_checkpoint_eq(&event_bus, &peer_id, Some(seq3 as u64)).await,
        "healthy fail-open peer must advance past the corrupt row to seq {seq3}"
    );

    // The fail-closed subscriber holds at the last good sequence.
    assert!(
        poll_checkpoint_eq(&event_bus, &fc_id, Some(seq1 as u64)).await,
        "fail-closed subscriber must hold its checkpoint at seq {seq1} (below the corrupt row)"
    );

    let fc_state = fc_store.get_state(stream).await.unwrap().unwrap();
    let fc_ids: Vec<Uuid> = fc_state.0.iter().map(|e| e.id).collect();
    assert_eq!(
        fc_ids,
        vec![valid1_id],
        "fail-closed applied only the pre-halt event"
    );

    let peer_state = peer_store.get_state(stream).await.unwrap().unwrap();
    let peer_ids: Vec<Uuid> = peer_state.0.iter().map(|e| e.id).collect();
    assert!(peer_ids.contains(&valid1_id) && peer_ids.contains(&valid3_id));
    assert!(!peer_ids.contains(&corrupt_id));

    // R8: a held wedge blocks readiness.
    let ready = event_bus
        .wait_until_caught_up(&fc_id, GapDuration::from_secs(2))
        .await
        .expect("wait_until_caught_up");
    assert!(
        !ready,
        "a held fail-closed subscriber must not certify as caught up"
    );

    // R7: deser DLQ row.
    let dlq = poll_dlq_rows(&pool, &fc_id).await;
    assert_eq!(
        dlq.len(),
        1,
        "exactly one deser DLQ row for the corrupt event"
    );
    assert_eq!(dlq[0].0, corrupt_id);
    assert_eq!(dlq[0].1, seq2);
    assert!(
        dlq[0].2.starts_with("unrecoverable: deserialize:"),
        "unexpected DLQ error_message: {}",
        dlq[0].2
    );

    // R7: on_halt fired exactly once, on entry.
    {
        let halts = halts.lock().unwrap();
        assert_eq!(
            halts.len(),
            1,
            "on_halt fires once on entry, not per re-attempt"
        );
        assert_eq!(halts[0].subscriber_id, fc_id);
        assert_eq!(halts[0].held_below_sequence, seq2 as u64);
        assert_eq!(halts[0].reason, HaltReason::DeserializeFailure);
    }

    event_bus.shutdown().await.expect("shutdown");
    drop_isolated_events_table(&pool, &table).await;
}

/// T2 (R6): after T1's deser halt, correcting the payload lets the next batch
/// apply the held event and everything after it exactly once, in order, with no
/// manual replay.
#[tokio::test]
#[serial]
async fn test_live_deser_halt_self_heals_after_fix() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    let table = isolated_events_table(&pool).await;

    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        gap_timeout: GapDuration::from_secs(30),
        events_table: table.clone(),
        ..Default::default()
    };
    let event_bus = start_isolated_bus(&pool, config).await;

    let fc_id = format!("projection:fc-heal:{}", Uuid::new_v4());
    let fc = TestProjection::with_subscriber_id(fc_id.clone()).fail_closed();
    let fc_store = fc.get_state_store().clone();
    event_bus
        .subscribe(ProjectionHandler::new(fc))
        .await
        .expect("subscribe fc");

    tokio::time::sleep(GapDuration::from_millis(100)).await;

    let stream = Uuid::new_v4();
    let (_valid1_id, seq1) = insert_committed_event(&pool, &table, stream, 1, "v1").await;
    let (_corrupt_id, seq2) = insert_corrupt_event(&pool, &table, stream, 2).await;
    let (_valid3_id, seq3) = insert_committed_event(&pool, &table, stream, 3, "v3").await;

    // Confirm the halt landed at seq1.
    assert!(
        poll_checkpoint_eq(&event_bus, &fc_id, Some(seq1 as u64)).await,
        "fail-closed subscriber must first halt at seq {seq1}"
    );

    // Fix the corrupt payload; the next batch self-heals.
    fix_event_payload(&pool, &table, seq2, "recovered").await;

    assert!(
        poll_checkpoint_eq(&event_bus, &fc_id, Some(seq3 as u64)).await,
        "after the fix the checkpoint must advance to the tail seq {seq3}"
    );

    let fc_state = fc_store.get_state(stream).await.unwrap().unwrap();
    let seqs: Vec<u64> = fc_state
        .0
        .iter()
        .map(|e| e.global_sequence.unwrap())
        .collect();
    assert_eq!(
        seqs,
        vec![seq1 as u64, seq2 as u64, seq3 as u64],
        "recovery applies the held event and everything after it exactly once, in order"
    );

    event_bus.shutdown().await.expect("shutdown");
    drop_isolated_events_table(&pool, &table).await;
}

/// T3 (R4, R6, R7): a fail-closed observer that exhausts its retries halts after
/// a DLQ row + `on_dlq_insertion` + `on_halt`; during the halt it is re-invoked
/// at most about once per batch cycle (never the full retry ladder), a healthy
/// peer is unaffected, and fixing the observer self-heals.
#[tokio::test]
#[serial]
async fn test_live_observer_failure_halt_fail_closed() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    let table = isolated_events_table(&pool).await;

    let halts = Arc::new(StdMutex::new(Vec::new()));
    let dlq_inserts = Arc::new(StdMutex::new(Vec::new()));
    let max_retries = 3u32;
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        max_retries,
        initial_retry_delay: GapDuration::from_millis(50),
        max_retry_delay: GapDuration::from_millis(200),
        gap_timeout: GapDuration::from_secs(30),
        events_table: table.clone(),
        on_halt: Some(Arc::new(CapturingHaltCallback {
            halts: halts.clone(),
        })),
        on_dlq_insertion: Some(Arc::new(CapturingDlqCallback {
            inserts: dlq_inserts.clone(),
        })),
        ..Default::default()
    };
    let event_bus = start_isolated_bus(&pool, config).await;

    let healthy = Arc::new(AtomicBool::new(false));
    let invocations = Arc::new(StdMutex::new(StdHashMap::new()));
    let applied = Arc::new(StdMutex::new(Vec::new()));
    let fc_id = format!("observer:fc-fail:{}", Uuid::new_v4());
    event_bus
        .subscribe(CountingFailingObserver {
            subscriber_id: fc_id.clone(),
            failure_mode: FailureMode::FailClosed,
            healthy: healthy.clone(),
            invocations: invocations.clone(),
            applied: applied.clone(),
        })
        .await
        .expect("subscribe failing observer");

    let peer_id = format!("projection:peer:{}", Uuid::new_v4());
    let peer = TestProjection::with_subscriber_id(peer_id.clone());
    event_bus
        .subscribe(ProjectionHandler::new(peer))
        .await
        .expect("subscribe peer");

    tokio::time::sleep(GapDuration::from_millis(100)).await;

    let stream = Uuid::new_v4();
    let (_e1, seq1) = insert_committed_event(&pool, &table, stream, 1, "e1").await;
    let (_e2, seq2) = insert_committed_event(&pool, &table, stream, 2, "e2").await;
    let (_e3, seq3) = insert_committed_event(&pool, &table, stream, 3, "e3").await;

    // The observer fails on the first event: DLQ row + callbacks + halt below seq1.
    let dlq = poll_dlq_rows(&pool, &fc_id).await;
    assert_eq!(dlq.len(), 1, "observer failure DLQs the failed event");
    assert_eq!(dlq[0].1, seq1);

    // Healthy peer is unaffected.
    assert!(
        poll_checkpoint_eq(&event_bus, &peer_id, Some(seq3 as u64)).await,
        "healthy peer advances to the tail while the observer is wedged"
    );

    // Fail-closed subscriber holds below the failing sequence (no checkpoint row).
    assert_eq!(
        event_bus
            .get_checkpoint(&fc_id)
            .await
            .expect("get checkpoint"),
        None,
        "the wedged fail-closed observer must not persist any checkpoint"
    );

    // on_dlq_insertion + on_halt both fired.
    assert!(
        dlq_inserts
            .lock()
            .unwrap()
            .iter()
            .any(|i| i.subscriber_id == fc_id && i.retry_count == max_retries + 1),
        "on_dlq_insertion fires with retry_count == max_retries + 1"
    );
    {
        let h = halts.lock().unwrap();
        assert_eq!(h.len(), 1, "on_halt fires once on entry");
        assert_eq!(h[0].subscriber_id, fc_id);
        assert_eq!(h[0].reason, HaltReason::ObserverFailure);
        assert_eq!(h[0].held_below_sequence, seq1 as u64);
    }

    // R4: the ladder ran once; during the hold the observer is re-invoked ~once
    // per batch cycle, never re-running the full ladder each cycle.
    let first_failure_count = seq_invocations(&invocations, seq1 as u64);
    assert!(
        first_failure_count > max_retries,
        "the first failure must run the full ladder ({} invocations), saw {first_failure_count}",
        max_retries + 1
    );
    let wait_secs = 3u32;
    tokio::time::sleep(GapDuration::from_secs(wait_secs as u64)).await;
    let held_count = seq_invocations(&invocations, seq1 as u64);
    assert!(
        held_count <= first_failure_count + wait_secs + 3,
        "held re-attempts must be ~1 per cycle: {held_count} > {first_failure_count} + {wait_secs} + slack"
    );
    assert!(
        held_count < first_failure_count * max_retries,
        "the retry ladder must not re-run every cycle: {held_count} >= {first_failure_count} * {max_retries}"
    );

    // R6: fixing the observer self-heals — the held event and everything after
    // it are applied exactly once, in order.
    healthy.store(true, Ordering::SeqCst);
    assert!(
        poll_checkpoint_eq(&event_bus, &fc_id, Some(seq3 as u64)).await,
        "after the observer recovers the checkpoint advances to the tail"
    );
    let applied = applied.lock().unwrap().clone();
    assert_eq!(
        applied,
        vec![seq1 as u64, seq2 as u64, seq3 as u64],
        "recovery applies every held-and-after event exactly once, in order"
    );

    event_bus.shutdown().await.expect("shutdown");
    drop_isolated_events_table(&pool, &table).await;
}

/// T8-live (R11): a panicking observer no longer kills the listener task. Under
/// fail-closed it halts (DLQ + `on_halt`); the listener stays alive and a
/// healthy peer keeps advancing.
#[tokio::test]
#[serial]
async fn test_live_panicking_observer_fail_closed_halts() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    let table = isolated_events_table(&pool).await;

    let halts = Arc::new(StdMutex::new(Vec::new()));
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        max_retries: 2,
        initial_retry_delay: GapDuration::from_millis(50),
        max_retry_delay: GapDuration::from_millis(100),
        gap_timeout: GapDuration::from_secs(30),
        events_table: table.clone(),
        on_halt: Some(Arc::new(CapturingHaltCallback {
            halts: halts.clone(),
        })),
        ..Default::default()
    };
    let event_bus = start_isolated_bus(&pool, config).await;

    let invocations = Arc::new(AtomicU32::new(0));
    let panic_id = format!("observer:panic-fc:{}", Uuid::new_v4());
    event_bus
        .subscribe(PanickingObserver {
            subscriber_id: panic_id.clone(),
            failure_mode: FailureMode::FailClosed,
            invocations: invocations.clone(),
        })
        .await
        .expect("subscribe panicking observer");

    let peer_id = format!("projection:peer:{}", Uuid::new_v4());
    let peer = TestProjection::with_subscriber_id(peer_id.clone());
    event_bus
        .subscribe(ProjectionHandler::new(peer))
        .await
        .expect("subscribe peer");

    tokio::time::sleep(GapDuration::from_millis(100)).await;

    let stream = Uuid::new_v4();
    let (_e1, seq1) = insert_committed_event(&pool, &table, stream, 1, "e1").await;

    // Panic is contained → DLQ + halt, not listener death.
    let dlq = poll_dlq_rows(&pool, &panic_id).await;
    assert_eq!(dlq[0].1, seq1);
    assert!(
        poll_checkpoint_eq(&event_bus, &peer_id, Some(seq1 as u64)).await,
        "listener survives the panic and keeps serving the healthy peer"
    );
    assert!(
        event_bus.is_running().await,
        "listener task must still be alive"
    );
    assert!(invocations.load(Ordering::SeqCst) >= 1);
    {
        let h = halts.lock().unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].reason, HaltReason::ObserverFailure);
    }

    event_bus.shutdown().await.expect("shutdown");
    drop_isolated_events_table(&pool, &table).await;
}

/// T8-live (R10/R11): under fail-open a panicking observer is contained and the
/// checkpoint advances past the event (documented behaviour change — the
/// listener survives where today it would die).
#[tokio::test]
#[serial]
async fn test_live_panicking_observer_fail_open_continues() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    let table = isolated_events_table(&pool).await;

    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        max_retries: 2,
        initial_retry_delay: GapDuration::from_millis(50),
        max_retry_delay: GapDuration::from_millis(100),
        gap_timeout: GapDuration::from_secs(30),
        events_table: table.clone(),
        ..Default::default()
    };
    let event_bus = start_isolated_bus(&pool, config).await;

    let invocations = Arc::new(AtomicU32::new(0));
    let panic_id = format!("observer:panic-fo:{}", Uuid::new_v4());
    event_bus
        .subscribe(PanickingObserver {
            subscriber_id: panic_id.clone(),
            failure_mode: FailureMode::FailOpen,
            invocations: invocations.clone(),
        })
        .await
        .expect("subscribe panicking observer");

    tokio::time::sleep(GapDuration::from_millis(100)).await;

    let stream = Uuid::new_v4();
    let (_e1, seq1) = insert_committed_event(&pool, &table, stream, 1, "e1").await;

    // DLQ + continue: the fail-open observer advances past the panicking event.
    let dlq = poll_dlq_rows(&pool, &panic_id).await;
    assert_eq!(dlq[0].1, seq1);
    assert!(
        poll_checkpoint_eq(&event_bus, &panic_id, Some(seq1 as u64)).await,
        "fail-open contains the panic and advances past the event"
    );
    assert!(
        event_bus.is_running().await,
        "listener task must still be alive"
    );

    event_bus.shutdown().await.expect("shutdown");
    drop_isolated_events_table(&pool, &table).await;
}

/// T6 (Batched interplay): in `Batched` mode a fail-closed wedge must not let the
/// periodic `max_delay_ms` flush publish above the held sequence; after the fix,
/// flushes resume.
#[tokio::test]
#[serial]
async fn test_live_batched_fail_closed_wedge_holds_then_resumes() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    let table = isolated_events_table(&pool).await;

    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        checkpoint_mode: epoch_pg::event_bus::CheckpointMode::Batched {
            batch_size: 1000,
            max_delay_ms: 200,
        },
        gap_timeout: GapDuration::from_secs(30),
        events_table: table.clone(),
        ..Default::default()
    };
    let event_bus = start_isolated_bus(&pool, config).await;

    let fc_id = format!("projection:fc-batched:{}", Uuid::new_v4());
    let fc = TestProjection::with_subscriber_id(fc_id.clone()).fail_closed();
    event_bus
        .subscribe(ProjectionHandler::new(fc))
        .await
        .expect("subscribe fc");

    tokio::time::sleep(GapDuration::from_millis(100)).await;

    let stream = Uuid::new_v4();
    let (_valid1_id, seq1) = insert_committed_event(&pool, &table, stream, 1, "v1").await;
    let (_corrupt_id, seq2) = insert_corrupt_event(&pool, &table, stream, 2).await;
    let (_valid3_id, seq3) = insert_committed_event(&pool, &table, stream, 3, "v3").await;

    // Halt lands at seq1.
    assert!(
        poll_checkpoint_eq(&event_bus, &fc_id, Some(seq1 as u64)).await,
        "batched fail-closed subscriber must hold at seq {seq1}"
    );

    // Across several max_delay periods the periodic flush must not publish above
    // the wedge.
    for _ in 0..8 {
        tokio::time::sleep(GapDuration::from_millis(200)).await;
        assert_eq!(
            event_bus
                .get_checkpoint(&fc_id)
                .await
                .expect("get checkpoint"),
            Some(seq1 as u64),
            "the batched timer flush must never publish above the held wedge"
        );
    }

    // After the fix the wedge clears and flushes resume.
    fix_event_payload(&pool, &table, seq2, "recovered").await;
    assert!(
        poll_checkpoint_eq(&event_bus, &fc_id, Some(seq3 as u64)).await,
        "after the fix the batched flush resumes and reaches seq {seq3}"
    );

    event_bus.shutdown().await.expect("shutdown");
    drop_isolated_events_table(&pool, &table).await;
}

// ============================================================================
// P3 (spec 0028): catch-up + drain fail-closed halt, inline panic containment.
//
// T4 (R3, R7, R8): a fail-closed subscriber halts at the bad sequence during
// the catch-up pass and the subscribe() buffer drain, without advancing, and a
// drain halt still completes registration so the subscriber self-heals under
// the live listener. Two catch-up variants exercise both the short-batch case
// and the FULL-batch case (which proves the P0 infinite-loop hazard is fixed:
// a spinning bus would hang the bounded subscribe() timeout below).
//
// T8 remainder (R11): a panic during catch-up is contained (halt + DLQ, no
// crash of subscribe()); a panic in the inline drain routes through publish()'s
// Err branch with inline_state cleaned up (no deadlock).
// ============================================================================

/// Builds a bus on an isolated `table` WITHOUT starting the live listener, so
/// the only path that can halt is the one under test (catch-up inside
/// `subscribe()`), giving clean single-halt `on_halt` assertions.
async fn build_isolated_bus_no_listener(
    pool: &PgPool,
    config: epoch_pg::event_bus::ReliableDeliveryConfig,
) -> PgEventBus<TestEventData> {
    let channel_name = format!("test_fc_nl_{}", Uuid::new_v4().simple());
    let event_bus = PgEventBus::<TestEventData>::with_config(pool.clone(), channel_name, config);
    event_bus
        .setup_trigger()
        .await
        .expect("Failed to setup trigger");
    event_bus
}

/// Pre-plants a persisted checkpoint row for `subscriber_id` on `table`'s bus,
/// so catch-up resumes from `seq` rather than replaying the whole table.
async fn plant_checkpoint(
    pool: &PgPool,
    table: &str,
    subscriber_id: &str,
    seq: i64,
    event_id: Uuid,
) {
    sqlx::query(
        r#"INSERT INTO epoch_event_bus_checkpoints
               (bus_name, subscriber_id, last_global_sequence, last_event_id, updated_at)
           VALUES ($1, $2, $3, $4, NOW())
           ON CONFLICT (bus_name, subscriber_id) DO UPDATE SET
               last_global_sequence = EXCLUDED.last_global_sequence,
               last_event_id = EXCLUDED.last_event_id,
               updated_at = NOW()"#,
    )
    .bind(table)
    .bind(subscriber_id)
    .bind(seq)
    .bind(event_id)
    .execute(pool)
    .await
    .expect("plant checkpoint");
}

/// T4 catch-up variant 1 (short-batch, R3/R7/R8): with a pre-planted checkpoint
/// and a corrupt row just above it, catch-up inside `subscribe()` halts at the
/// bad sequence — no advance, DLQ row, `on_halt`, readiness blocked. The batch
/// containing the corrupt row is short (< catch_up_batch_size), so this variant
/// would pass even with the P0 hazard present; the full-batch variant below is
/// what proves the fix.
#[tokio::test]
#[serial]
async fn test_catchup_deser_halt_short_batch_fail_closed() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    let table = isolated_events_table(&pool).await;

    let halts = Arc::new(StdMutex::new(Vec::new()));
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        gap_timeout: GapDuration::from_secs(30),
        events_table: table.clone(),
        on_halt: Some(Arc::new(CapturingHaltCallback {
            halts: halts.clone(),
        })),
        ..Default::default()
    };
    let event_bus = build_isolated_bus_no_listener(&pool, config).await;

    let stream = Uuid::new_v4();
    let (valid1_id, seq1) = insert_committed_event(&pool, &table, stream, 1, "v1").await;
    let (corrupt_id, seq2) = insert_corrupt_event(&pool, &table, stream, 2).await;
    let (_valid3_id, _seq3) = insert_committed_event(&pool, &table, stream, 3, "v3").await;

    let fc_id = format!("projection:fc-catchup-short:{}", Uuid::new_v4());
    // Pre-plant the checkpoint at seq1 so catch-up resumes just below the corrupt row.
    plant_checkpoint(&pool, &table, &fc_id, seq1, valid1_id).await;

    let fc = TestProjection::with_subscriber_id(fc_id.clone()).fail_closed();
    let fc_store = fc.get_state_store().clone();
    event_bus
        .subscribe(ProjectionHandler::new(fc))
        .await
        .expect("subscribe fc (catch-up halt must not error out)");

    // No advance past the corrupt row: checkpoint held at seq1.
    assert_eq!(
        event_bus
            .get_checkpoint(&fc_id)
            .await
            .expect("get checkpoint"),
        Some(seq1 as u64),
        "catch-up must hold the checkpoint at seq {seq1}, below the corrupt row"
    );

    // The corrupt row and everything after it stayed unapplied.
    assert!(
        fc_store.get_state(stream).await.unwrap().is_none(),
        "catch-up applied nothing above the pre-planted checkpoint"
    );

    // R8: readiness blocked.
    assert!(
        !event_bus
            .wait_until_caught_up(&fc_id, GapDuration::from_secs(2))
            .await
            .expect("wait_until_caught_up"),
        "a held fail-closed subscriber must not certify as caught up"
    );

    // R7: deser DLQ row + on_halt on entry.
    let dlq = read_dlq_rows(&pool, &fc_id).await;
    assert_eq!(
        dlq.len(),
        1,
        "exactly one deser DLQ row for the corrupt event"
    );
    assert_eq!(dlq[0].0, corrupt_id);
    assert_eq!(dlq[0].1, seq2);
    assert!(dlq[0].2.starts_with("unrecoverable: deserialize:"));
    {
        let h = halts.lock().unwrap();
        assert_eq!(h.len(), 1, "on_halt fires once, on entry");
        assert_eq!(h[0].subscriber_id, fc_id);
        assert_eq!(h[0].held_below_sequence, seq2 as u64);
        assert_eq!(h[0].reason, HaltReason::DeserializeFailure);
    }

    drop_isolated_events_table(&pool, &table).await;
}

/// T4 catch-up variant 2 (FULL-batch, R3/R7/R8 + P0 hazard): more than
/// `catch_up_batch_size` (100) rows sit below the corrupt one, so the batch that
/// contains the corrupt row is full. Before the P0 fix, halting only the inner
/// row loop on a full batch re-fetches the identical window forever and
/// `subscribe()` never returns; the bounded timeout below turns that spin into a
/// clear test failure instead of a hang.
#[tokio::test]
#[serial]
async fn test_catchup_deser_halt_full_batch_fail_closed() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    let table = isolated_events_table(&pool).await;

    let halts = Arc::new(StdMutex::new(Vec::new()));
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        // Default catch_up_batch_size is 100; keep it so the "full batch" is 100.
        gap_timeout: GapDuration::from_secs(30),
        events_table: table.clone(),
        on_halt: Some(Arc::new(CapturingHaltCallback {
            halts: halts.clone(),
        })),
        ..Default::default()
    };
    let batch = config.catch_up_batch_size;
    let event_bus = build_isolated_bus_no_listener(&pool, config).await;

    let stream = Uuid::new_v4();
    // valid(seq1), corrupt(seq2), then > catch_up_batch_size valid rows below it,
    // so the first LIMIT `batch` page is full and contains the corrupt row.
    let (_valid1_id, seq1) = insert_committed_event(&pool, &table, stream, 1, "v1").await;
    let (corrupt_id, seq2) = insert_corrupt_event(&pool, &table, stream, 2).await;
    for i in 0..(batch as i64 + 5) {
        insert_committed_event(&pool, &table, stream, 3 + i, "tail").await;
    }

    let fc_id = format!("projection:fc-catchup-full:{}", Uuid::new_v4());
    let fc = TestProjection::with_subscriber_id(fc_id.clone()).fail_closed();

    // A spinning catch-up would never let subscribe() return; bound it.
    let subscribe_fut = event_bus.subscribe(ProjectionHandler::new(fc));
    tokio::time::timeout(GapDuration::from_secs(30), subscribe_fut)
        .await
        .expect(
            "subscribe() did not return within 30s — the catch-up pagination loop is spinning on a \
             full batch (P0 infinite-loop hazard is not fixed)",
        )
        .expect("subscribe fc (catch-up halt must not error out)");

    // Held at seq1 despite 100+ rows below the corrupt one.
    assert_eq!(
        event_bus
            .get_checkpoint(&fc_id)
            .await
            .expect("get checkpoint"),
        Some(seq1 as u64),
        "full-batch catch-up must still hold at seq {seq1}, below the corrupt row"
    );

    assert!(
        !event_bus
            .wait_until_caught_up(&fc_id, GapDuration::from_secs(2))
            .await
            .expect("wait_until_caught_up"),
        "a held fail-closed subscriber must not certify as caught up"
    );

    let dlq = read_dlq_rows(&pool, &fc_id).await;
    assert_eq!(
        dlq.len(),
        1,
        "exactly one deser DLQ row for the corrupt event"
    );
    assert_eq!(dlq[0].0, corrupt_id);
    assert_eq!(dlq[0].1, seq2);
    {
        let h = halts.lock().unwrap();
        assert_eq!(h.len(), 1, "on_halt fires once, on entry");
        assert_eq!(h[0].reason, HaltReason::DeserializeFailure);
        assert_eq!(h[0].held_below_sequence, seq2 as u64);
    }

    drop_isolated_events_table(&pool, &table).await;
}

/// T4 drain leg (R3): a fail-closed halt during the subscribe() catch-up/buffer
/// drain phase must NOT early-return out of subscribe() — registration
/// completes, so the live listener drives the subscriber afterward and it
/// self-heals once the payload is fixed. A small `catch_up_batch_size` plus
/// events committed concurrently during catch-up pushes the corrupt row through
/// the buffer drain; whichever path (catch-up or drain) catches it, fail-closed
/// holds below it and never advances past it.
#[tokio::test]
#[serial]
async fn test_drain_halt_registers_and_self_heals_fail_closed() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    let table = isolated_events_table(&pool).await;

    let halts = Arc::new(StdMutex::new(Vec::new()));
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        catch_up_batch_size: 2,
        gap_timeout: GapDuration::from_secs(30),
        events_table: table.clone(),
        on_halt: Some(Arc::new(CapturingHaltCallback {
            halts: halts.clone(),
        })),
        ..Default::default()
    };
    let event_bus = start_isolated_bus(&pool, config).await;

    // Pre-existing rows so catch-up takes several DB round-trips at batch_size=2,
    // widening the window for the during-task's rows to land in the buffer drain.
    let stream = Uuid::new_v4();
    let mut last_pre_seq = 0i64;
    for i in 1i64..=20 {
        let (_id, s) = insert_committed_event(&pool, &table, stream, i, "pre").await;
        last_pre_seq = s;
    }

    // Concurrently commit valid → corrupt → valid while catch-up runs.
    let pool2 = pool.clone();
    let table2 = table.clone();
    let during = tokio::spawn(async move {
        tokio::time::sleep(GapDuration::from_millis(5)).await;
        let (_v1, s1) = insert_committed_event(&pool2, &table2, stream, 21, "d1").await;
        let (corrupt_id, sc) = insert_corrupt_event(&pool2, &table2, stream, 22).await;
        let (_v2, s2) = insert_committed_event(&pool2, &table2, stream, 23, "d2").await;
        (s1, corrupt_id, sc, s2)
    });

    let fc_id = format!("projection:fc-drain:{}", Uuid::new_v4());
    let fc = TestProjection::with_subscriber_id(fc_id.clone()).fail_closed();
    // R3: a drain halt must not turn subscribe() into an Err / early return.
    event_bus
        .subscribe(ProjectionHandler::new(fc))
        .await
        .expect("subscribe fc (drain halt must still complete registration)");

    let (s1, corrupt_id, sc, s2) = during.await.expect("during task");
    assert!(
        sc > last_pre_seq,
        "corrupt row must sit above the pre-existing rows"
    );

    // Fail-closed never advances past the corrupt row on any path — the
    // checkpoint holds at the last good sequence below it.
    assert!(
        poll_checkpoint_eq(&event_bus, &fc_id, Some(s1 as u64)).await,
        "fail-closed must hold the checkpoint at seq {s1}, below the corrupt row {sc}"
    );

    // R7: DLQ row for the corrupt event + on_halt fired (at least once — a halt
    // can be re-entered by the live listener after a catch-up/drain halt).
    let dlq = poll_dlq_rows(&pool, &fc_id).await;
    assert!(
        dlq.iter().any(|(id, seq, msg)| *id == corrupt_id
            && *seq == sc
            && msg.starts_with("unrecoverable: deserialize:")),
        "a deser DLQ row must exist for the corrupt event, got {dlq:?}"
    );
    assert!(
        halts
            .lock()
            .unwrap()
            .iter()
            .any(|h| h.subscriber_id == fc_id
                && h.reason == HaltReason::DeserializeFailure
                && h.held_below_sequence == sc as u64),
        "on_halt must fire for the drain/catch-up deser halt"
    );

    // R8: readiness blocked while held.
    assert!(
        !event_bus
            .wait_until_caught_up(&fc_id, GapDuration::from_secs(2))
            .await
            .expect("wait_until_caught_up"),
        "a held fail-closed subscriber must not certify as caught up"
    );

    // R3 + R6: because registration completed, the live listener drives the
    // subscriber, so fixing the payload self-heals it up to the tail.
    fix_event_payload(&pool, &table, sc, "recovered").await;
    assert!(
        poll_checkpoint_eq(&event_bus, &fc_id, Some(s2 as u64)).await,
        "after the fix the registered subscriber self-heals to seq {s2}"
    );

    event_bus.shutdown().await.expect("shutdown");
    drop_isolated_events_table(&pool, &table).await;
}

/// T8 catch-up leg (R11): a panicking observer during the catch-up pass is
/// contained (classified as an observer failure → DLQ → fail-closed halt);
/// `subscribe()` returns normally rather than unwinding the panic to the caller.
#[tokio::test]
#[serial]
async fn test_catchup_panicking_observer_contained_fail_closed() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    let table = isolated_events_table(&pool).await;

    let halts = Arc::new(StdMutex::new(Vec::new()));
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        max_retries: 1,
        initial_retry_delay: GapDuration::from_millis(20),
        max_retry_delay: GapDuration::from_millis(40),
        gap_timeout: GapDuration::from_secs(30),
        events_table: table.clone(),
        on_halt: Some(Arc::new(CapturingHaltCallback {
            halts: halts.clone(),
        })),
        ..Default::default()
    };
    let event_bus = build_isolated_bus_no_listener(&pool, config).await;

    let stream = Uuid::new_v4();
    let (_e1, seq1) = insert_committed_event(&pool, &table, stream, 1, "e1").await;

    let invocations = Arc::new(AtomicU32::new(0));
    let panic_id = format!("observer:catchup-panic-fc:{}", Uuid::new_v4());
    // A panic in catch-up must not unwind through subscribe(): it returns Ok,
    // with the halt surfaced via the DLQ + on_halt.
    event_bus
        .subscribe(PanickingObserver {
            subscriber_id: panic_id.clone(),
            failure_mode: FailureMode::FailClosed,
            invocations: invocations.clone(),
        })
        .await
        .expect("subscribe panicking observer (catch-up panic must be contained)");

    assert!(
        invocations.load(Ordering::SeqCst) >= 1,
        "the observer was invoked during catch-up"
    );
    // Held below seq1: no checkpoint persisted.
    assert_eq!(
        event_bus
            .get_checkpoint(&panic_id)
            .await
            .expect("get checkpoint"),
        None,
        "the wedged fail-closed observer must not persist a checkpoint"
    );
    let dlq = read_dlq_rows(&pool, &panic_id).await;
    assert_eq!(dlq.len(), 1, "the panicking event is DLQ'd");
    assert_eq!(dlq[0].1, seq1);
    {
        let h = halts.lock().unwrap();
        assert_eq!(h.len(), 1, "on_halt fires once on entry");
        assert_eq!(h[0].reason, HaltReason::ObserverFailure);
        assert_eq!(h[0].held_below_sequence, seq1 as u64);
    }

    drop_isolated_events_table(&pool, &table).await;
}

/// T8 inline leg (R11): a panic in the inline drain is caught and routed through
/// `publish()`'s existing `Err` branch, with `inline_state` cleaned up — a
/// subsequent `publish()` must not deadlock on stuck `in_progress`/unnotified
/// waiters.
#[tokio::test]
#[serial]
async fn test_inline_drain_panic_returns_err_no_deadlock() {
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

    let channel_name = format!("inline_panic_{}", Uuid::new_v4().simple());
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        dispatch_mode: epoch_pg::event_bus::DispatchMode::Inline,
        ..Default::default()
    };
    let event_bus: PgEventBus<TestEventData> =
        PgEventBus::with_config(pool.clone(), channel_name, config);

    let invocations = Arc::new(AtomicU32::new(0));
    let panic_id = format!("observer:inline-panic:{}", Uuid::new_v4());
    event_bus
        .subscribe(PanickingObserver {
            subscriber_id: panic_id.clone(),
            failure_mode: FailureMode::FailClosed,
            invocations: invocations.clone(),
        })
        .await
        .expect("subscribe panicking observer");

    let stream = Uuid::new_v4();
    let first = event_bus
        .publish(Arc::new(new_event(stream, 1, "boom1")))
        .await;
    assert!(
        first.is_err(),
        "a panic in the inline drain must surface as Err from publish(), got {first:?}"
    );

    // inline_state must be cleaned up (in_progress reset, queue cleared, waiters
    // notified): a second publish returns promptly instead of deadlocking.
    let second = tokio::time::timeout(
        GapDuration::from_secs(5),
        event_bus.publish(Arc::new(new_event(stream, 2, "boom2"))),
    )
    .await
    .expect("second publish() deadlocked — inline_state was not cleaned up after the panic");
    assert!(
        second.is_err(),
        "the observer still panics on the second event, so publish() is Err again, got {second:?}"
    );
    assert!(
        invocations.load(Ordering::SeqCst) >= 2,
        "both publishes reached the observer"
    );
}

/// T5 (R5, R7, spec 0028 P4): a fail-closed subscriber refuses the gap-timeout
/// backstop — no checkpoint advance, no `epoch_event_bus_gap_timeouts` row, no
/// `on_gap_timeout`, `on_halt(GapUnproven)` fires exactly once.
///
/// Recovery leg: rolling back the held transaction (so the sequence provably
/// never existed) clears the snapshot fence, and the subscriber's checkpoint
/// then advances past the gap via `FenceCleared` (not the backstop).
///
/// Uses an isolated table + sequence (`snapshot_fencing: true`, `gap_timeout:
/// 500ms`) so the hole is deterministic and does not stall sibling test binaries.
/// A generous recovery-poll timeout (30 s) accommodates xmin advancement on a
/// shared test instance.
#[tokio::test]
#[serial]
async fn test_fail_closed_gap_refusal_and_fence_cleared_recovery() {
    use std::time::Duration as GapDuration;
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

    let table = isolated_events_table(&pool).await;
    let halts = Arc::new(StdMutex::new(Vec::new()));
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        snapshot_fencing: true,
        gap_timeout: GapDuration::from_millis(500),
        events_table: table.clone(),
        on_halt: Some(Arc::new(CapturingHaltCallback {
            halts: halts.clone(),
        })),
        ..Default::default()
    };
    let event_bus = start_isolated_bus(&pool, config).await;

    // Subscribe a fail-closed observer. Subscribe after start_listener so it
    // takes the live path (no catch-up) — the checkpoint is pre-planted just
    // below the hole to isolate from shared-table history.
    let fc_id = format!("projection:fc-gap:{}", Uuid::new_v4());
    let fc = TestProjection::with_subscriber_id(fc_id.clone()).fail_closed();
    event_bus
        .subscribe(ProjectionHandler::new(fc))
        .await
        .expect("subscribe fail-closed observer");
    tokio::time::sleep(GapDuration::from_millis(100)).await;

    // Commit a below-hole event; wait until the subscriber's checkpoint lands on
    // it before opening the hole.  This proves the write path is alive and gives
    // us the exact contiguous position the gap refusal must hold.
    let stream = Uuid::new_v4();
    let (_, seq_below) = insert_committed_event(&pool, &table, stream, 1, "below_hole").await;

    assert!(
        poll_checkpoint_eq(&event_bus, &fc_id, Some(seq_below as u64)).await,
        "fail-closed subscriber must reach seq {seq_below} before the hole is opened"
    );

    // Open the hole: claim an uncommitted sequence on the isolated table.
    let hole_stream = Uuid::new_v4();
    let mut tx_hole = pool.begin().await.expect("begin hole tx");
    let (_hole_id, seq_hole) = claim_hole_uncommitted(&mut tx_hole, &table, hole_stream).await;
    assert!(
        seq_hole > seq_below,
        "hole must sit above the below-hole event"
    );

    // Commit two events above the hole so the bus sees a gap at seq_hole.
    let (_, seq_above1) = insert_committed_event(&pool, &table, hole_stream, 2, "above1").await;
    let (_, seq_above2) = insert_committed_event(&pool, &table, hole_stream, 3, "above2").await;
    assert!(seq_above1 > seq_hole && seq_above2 > seq_above1);

    // Wait for gap_timeout (500ms) + several bus cycles (1s flush_interval +
    // buffer) so fail-open would have advanced past the hole by now.
    tokio::time::sleep(GapDuration::from_millis(2500)).await;

    // --- Assert the gap was REFUSED (fail-closed) ---------------------------

    // Checkpoint must be held at seq_below (not advanced past seq_hole).
    let checkpoint_held = event_bus
        .get_checkpoint(&fc_id)
        .await
        .expect("get checkpoint");
    assert_eq!(
        checkpoint_held,
        Some(seq_below as u64),
        "fail-closed subscriber must hold checkpoint at seq {seq_below}, not advance past \
         hole at {seq_hole}; got {checkpoint_held:?}"
    );

    // No gap-timeout row must have been inserted (the backstop was refused).
    let gap_rows = event_bus
        .list_gap_timeouts(Some(&fc_id), false, 0, 50)
        .await
        .expect("list_gap_timeouts");
    assert!(
        gap_rows.is_empty(),
        "FailClosed must not insert a gap-timeout row; found: {gap_rows:?}"
    );

    // on_halt(GapUnproven) must have fired exactly once (halt entry only).
    {
        let halts_guard = halts.lock().unwrap();
        assert_eq!(
            halts_guard.len(),
            1,
            "on_halt must fire exactly once on halt entry, got: {halts_guard:?}"
        );
        assert_eq!(halts_guard[0].subscriber_id, fc_id);
        assert_eq!(halts_guard[0].held_below_sequence, seq_hole as u64);
        assert_eq!(halts_guard[0].reason, HaltReason::GapUnproven);
    }

    // --- Recovery: roll back the hole → FenceCleared → advance ---------------

    // Rolling back aborts the writer: the sequence seq_hole provably never
    // existed. On the next bus cycle the snapshot fence clears (xmin catches up
    // past fence_xmax), and the FenceCleared branch advances the checkpoint.
    tx_hole.rollback().await.expect("rollback hole tx");

    // Poll until the checkpoint advances past seq_hole (up to 30 s to allow
    // xmin to advance on a loaded instance).
    let mut recovered = false;
    for _ in 0..120 {
        let cp = event_bus
            .get_checkpoint(&fc_id)
            .await
            .expect("get checkpoint after rollback");
        if matches!(cp, Some(s) if s >= seq_above2 as u64) {
            recovered = true;
            break;
        }
        tokio::time::sleep(GapDuration::from_millis(250)).await;
    }
    assert!(
        recovered,
        "after rolling back the hole, the fail-closed subscriber must recover via \
         FenceCleared and advance to seq {seq_above2}"
    );

    // Still no gap-timeout row after recovery (FenceCleared is lossless and
    // never recorded).
    let gap_rows_after = event_bus
        .list_gap_timeouts(Some(&fc_id), false, 0, 50)
        .await
        .expect("list_gap_timeouts after recovery");
    assert!(
        gap_rows_after.is_empty(),
        "FenceCleared recovery must not insert a gap-timeout row; found: {gap_rows_after:?}"
    );

    event_bus.shutdown().await.expect("shutdown");
    drop_isolated_events_table(&pool, &table).await;
}

/// Fail-open gap regression pin (R10): with `snapshot_fencing: false` and
/// `gap_timeout: 500ms`, a fail-open subscriber's backstop DOES advance past
/// the gap, inserts a `epoch_event_bus_gap_timeouts` row, and invokes
/// `on_gap_timeout`. Byte-identical to pre-P4 behaviour.
///
/// This pin is complementary to the existing `test_gap_timeout_inserts_record`
/// and `test_gap_timeout_callback_is_invoked` suites; it runs on an isolated
/// table to avoid leaving holes on the shared sequence.
#[tokio::test]
#[serial]
async fn test_fail_open_gap_backstop_still_advances_pin() {
    use epoch_pg::event_bus::{GapTimeoutCallback, GapTimeoutInfo};
    use std::time::Duration as GapDuration;

    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

    let table = isolated_events_table(&pool).await;

    struct CapturingGapCallback {
        fired: Arc<StdMutex<Vec<u64>>>,
    }
    #[async_trait]
    impl GapTimeoutCallback for CapturingGapCallback {
        async fn on_gap_timeout(&self, info: GapTimeoutInfo) {
            self.fired.lock().unwrap().push(info.skipped_sequence);
        }
    }

    let fired = Arc::new(StdMutex::new(Vec::new()));
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        snapshot_fencing: false,
        gap_timeout: GapDuration::from_millis(500),
        events_table: table.clone(),
        on_gap_timeout: Some(Arc::new(CapturingGapCallback {
            fired: fired.clone(),
        })),
        ..Default::default()
    };
    let event_bus = start_isolated_bus(&pool, config).await;

    let fo_id = format!("projection:fo-gap-pin:{}", Uuid::new_v4());
    let fo = TestProjection::with_subscriber_id(fo_id.clone()); // FailOpen default
    event_bus
        .subscribe(ProjectionHandler::new(fo))
        .await
        .expect("subscribe fail-open observer");
    tokio::time::sleep(GapDuration::from_millis(100)).await;

    let stream = Uuid::new_v4();
    let (_, seq_below) = insert_committed_event(&pool, &table, stream, 1, "below_hole").await;
    assert!(
        poll_checkpoint_eq(&event_bus, &fo_id, Some(seq_below as u64)).await,
        "fail-open subscriber must reach seq {seq_below} before the hole"
    );

    let hole_stream = Uuid::new_v4();
    let mut tx_hole = pool.begin().await.expect("begin hole tx");
    let (_, seq_hole) = claim_hole_uncommitted(&mut tx_hole, &table, hole_stream).await;
    let (_, seq_above) = insert_committed_event(&pool, &table, hole_stream, 2, "above").await;
    assert!(seq_above > seq_hole);

    // Wait for the backstop to fire (gap_timeout 500ms + bus cycles).
    let mut advanced = false;
    for _ in 0..60 {
        let cp = event_bus
            .get_checkpoint(&fo_id)
            .await
            .expect("get checkpoint");
        if matches!(cp, Some(s) if s >= seq_above as u64) {
            advanced = true;
            break;
        }
        tokio::time::sleep(GapDuration::from_millis(100)).await;
    }
    // Release the hole before asserting so a failing test doesn't leave it open.
    tx_hole.rollback().await.expect("rollback hole tx");

    assert!(
        advanced,
        "fail-open subscriber must advance past the gap at {seq_hole} via backstop"
    );

    // A gap-timeout row must exist.
    let rows = event_bus
        .list_gap_timeouts(Some(&fo_id), false, 0, 50)
        .await
        .expect("list_gap_timeouts");
    assert!(
        rows.iter().any(|r| r.skipped_sequence == seq_hole as u64),
        "fail-open backstop must insert a gap-timeout row for seq {seq_hole}; got {rows:?}"
    );

    // on_gap_timeout callback must have fired.
    assert!(
        fired.lock().unwrap().contains(&(seq_hole as u64)),
        "fail-open on_gap_timeout must fire for seq {seq_hole}"
    );

    event_bus.shutdown().await.expect("shutdown");
    drop_isolated_events_table(&pool, &table).await;
}
