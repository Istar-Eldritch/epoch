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

struct TestProjection {
    state_store: InMemoryStateStore<TestState>,
    subscriber_id: String,
    subscription_mode: SubscriptionMode,
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
        }
    }

    /// A ReplayAlways projection: replays from 0 every process start and never
    /// reads or writes a persisted checkpoint (R5).
    pub fn replay_always(subscriber_id: String) -> Self {
        TestProjection {
            state_store: InMemoryStateStore::new(),
            subscriber_id,
            subscription_mode: SubscriptionMode::ReplayAlways,
        }
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

    let channel_name = format!("test_channel_{}", Uuid::new_v4().simple());
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
    let channel_name = format!("test_batch_channel_{}", Uuid::new_v4().simple());
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
    let channel_name = format!("test_coord_channel_{}", Uuid::new_v4().simple());
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
    let channel_name = format!("test_coord_skip_channel_{}", Uuid::new_v4().simple());
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
    let channel_name = format!("test_coord_multi_channel_{}", Uuid::new_v4().simple());
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
    common::truncate_epoch_tables(&pool).await;

    // Configure batched mode with batch_size=5 and long max_delay (won't trigger)
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        checkpoint_mode: epoch_pg::event_bus::CheckpointMode::Batched {
            batch_size: 5,
            max_delay_ms: 60000, // 60 seconds - won't trigger
        },
        ..Default::default()
    };

    let channel_name = format!("test_batched_bs_{}", Uuid::new_v4().simple());
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

    let event_store = PgEventStore::new(pool.clone(), event_bus.clone());

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

    // Store 4 events - should NOT trigger checkpoint flush yet
    for i in 1..=4 {
        let event = new_event(stream_id, i, &format!("batched_event_{}", i));
        event_store
            .store_event(event)
            .await
            .expect("Failed to store event");
    }

    // Give time for events to be processed
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    // Check checkpoint - it might or might not be written yet (depends on timing)
    // What we can verify is that after 5 events, it WILL be written

    // Store the 5th event - should trigger checkpoint flush
    let event = new_event(stream_id, 5, "batched_event_5");
    event_store
        .store_event(event)
        .await
        .expect("Failed to store event");

    // Give time for the checkpoint to be flushed
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    // Now the checkpoint should definitely be written
    let checkpoint = event_bus
        .get_checkpoint(&subscriber_id)
        .await
        .expect("Failed to get checkpoint");
    assert!(
        checkpoint.is_some(),
        "Checkpoint should be written after 5 events in batched mode"
    );
    assert!(checkpoint.unwrap() >= 5, "Checkpoint should be at least 5");
}

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
    common::truncate_epoch_tables(&pool).await;

    // Configure batched mode with large batch_size (won't trigger) and short max_delay
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        checkpoint_mode: epoch_pg::event_bus::CheckpointMode::Batched {
            batch_size: 1000,  // Won't reach this
            max_delay_ms: 500, // 500ms - will trigger
        },
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

    let event_store = PgEventStore::new(pool.clone(), event_bus.clone());

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

    // Store 3 events - won't reach batch_size
    for i in 1..=3 {
        let event = new_event(stream_id, i, &format!("delay_event_{}", i));
        event_store
            .store_event(event)
            .await
            .expect("Failed to store event");
    }

    // Give time for events to be processed but not for max_delay to trigger
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Now wait for the max_delay to trigger (flush_interval is 1s, max_delay is 500ms)
    // The periodic flush should happen within ~1.5 seconds
    tokio::time::sleep(tokio::time::Duration::from_millis(1500)).await;

    // Checkpoint should be written due to max_delay
    let checkpoint = event_bus
        .get_checkpoint(&subscriber_id)
        .await
        .expect("Failed to get checkpoint");
    assert!(
        checkpoint.is_some(),
        "Checkpoint should be written after max_delay in batched mode"
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
    let channel_name = format!("test_batched_catchup_{}", Uuid::new_v4().simple());

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

#[tokio::test]
#[serial]
async fn test_synchronous_checkpoint_still_works() {
    // Verify that synchronous mode (default) still works correctly
    common::init_test_logger();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };

    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");
    common::truncate_epoch_tables(&pool).await;

    // Explicitly use Synchronous mode
    let config = epoch_pg::event_bus::ReliableDeliveryConfig {
        checkpoint_mode: epoch_pg::event_bus::CheckpointMode::Synchronous,
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

    let event_store = PgEventStore::new(pool.clone(), event_bus.clone());

    let subscriber_id = format!("projection:sync_test:{}", Uuid::new_v4());
    let projection = TestProjection::with_subscriber_id(subscriber_id.clone());

    event_bus
        .subscribe(ProjectionHandler::new(projection))
        .await
        .expect("Failed to subscribe");

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let stream_id = Uuid::new_v4();

    // Store a single event
    let event = new_event(stream_id, 1, "sync_event");
    event_store
        .store_event(event)
        .await
        .expect("Failed to store event");

    // Give time for event to be processed
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    // In synchronous mode, checkpoint should be written immediately after each event
    let checkpoint = event_bus
        .get_checkpoint(&subscriber_id)
        .await
        .expect("Failed to get checkpoint");
    assert!(
        checkpoint.is_some(),
        "Checkpoint should be written immediately in synchronous mode"
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

    let channel_name = format!("test_channel_{}", Uuid::new_v4().simple());
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

    let channel_name = format!("test_shutdown_flush_{}", Uuid::new_v4().simple());
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
    let channel_name = format!("test_gap_resolve_{}", Uuid::new_v4().simple());
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

    let channel_name = format!("test_batched_gap_{}", Uuid::new_v4().simple());
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

    let channel_name = format!("test_shutdown_states_{}", Uuid::new_v4().simple());
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

/// Inserts a committed event for `stream_id` and returns its assigned
/// `global_sequence`.
async fn insert_committed_event(
    pool: &PgPool,
    stream_id: Uuid,
    version: i64,
    value: &str,
) -> (Uuid, i64) {
    let id = Uuid::new_v4();
    let data = serde_json::to_value(Some(TestEventData::TestEvent {
        value: value.to_string(),
    }))
    .unwrap();
    let seq: i64 = sqlx::query_scalar(
        r#"INSERT INTO epoch_events (id, stream_id, stream_version, event_type, data, created_at)
           VALUES ($1, $2, $3, 'MyEvent', $4, NOW())
           RETURNING global_sequence"#,
    )
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
async fn create_sequence_gap(pool: &PgPool, stream_id: Uuid) -> (Uuid, u64, Uuid) {
    let (before_id, _before_seq) = insert_committed_event(pool, stream_id, 1, "gap_before").await;

    // A rolled-back transaction consumes the next sequence value, which will
    // never commit — producing a permanent hole the subscriber must skip.
    let skipped_seq: i64 = {
        let mut tx = pool.begin().await.expect("begin rollback tx");
        let data = serde_json::to_value(Some(TestEventData::TestEvent {
            value: "rolled_back".to_string(),
        }))
        .unwrap();
        let seq: i64 = sqlx::query_scalar(
            r#"INSERT INTO epoch_events (id, stream_id, stream_version, event_type, data, created_at)
               VALUES ($1, $2, 2, 'MyEvent', $3, NOW())
               RETURNING global_sequence"#,
        )
        .bind(Uuid::new_v4())
        .bind(stream_id)
        .bind(&data)
        .fetch_one(&mut *tx)
        .await
        .expect("insert rolled-back event");
        tx.rollback().await.expect("rollback gap tx");
        seq
    };

    let (after_id, _after_seq) = insert_committed_event(pool, stream_id, 3, "gap_after").await;

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
    let channel_name = format!("test_gap_obs_{}", Uuid::new_v4().simple());
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
    let (before_id, skipped_seq, after_id) = create_sequence_gap(&pool, stream_id).await;

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
    let (_before_id, skipped_seq, _after_id) = create_sequence_gap(&pool, stream_id).await;

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
    let (_before_id, skipped_seq, _after_id) = create_sequence_gap(&pool, stream_id).await;

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
    let (_before_id, skipped_seq, _after_id) = create_sequence_gap(&pool, stream_id).await;

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
    let (_before_id, skipped_seq, _after_id) = create_sequence_gap(&pool, stream_id).await;

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
    let channel_name = format!("test_drain_page_{}", Uuid::new_v4().simple());
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
    let channel_name = format!("test_drain_deser_{}", Uuid::new_v4().simple());
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
    let channel_name = format!("test_fence_{}", Uuid::new_v4().simple());
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
    let (after_id, seq_after) = insert_committed_event(&pool, stream_id, 2, "after_gap").await;
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
    let (before_id, skipped_seq, after_id) = create_sequence_gap(&pool, stream_id).await;

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
    let (_before_id, skipped_seq, _after_id) = create_sequence_gap(&pool, stream_id).await;

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
    let (before_id, skipped_seq, after_id) = create_sequence_gap(&pool, stream_id).await;

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

    let channel_name = format!("test_channel_{}", Uuid::new_v4().simple());
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

    let channel_name = format!("test_channel_{}", Uuid::new_v4().simple());
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
    is_released: Arc<std::sync::atomic::AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
}

impl GatedObserver {
    /// Returns the observer plus the flag/notify pair the test uses to release
    /// it later. Set the flag and call `notify_waiters()` to release: events
    /// already blocked in `on_event` wake immediately, and any event that
    /// arrives after release sees the flag set and never blocks.
    fn new(
        subscriber_id: String,
    ) -> (
        Self,
        Arc<std::sync::atomic::AtomicBool>,
        Arc<tokio::sync::Notify>,
    ) {
        let is_released = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notify = Arc::new(tokio::sync::Notify::new());
        (
            Self {
                subscriber_id,
                is_released: is_released.clone(),
                notify: notify.clone(),
            },
            is_released,
            notify,
        )
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
        _event: Arc<Event<TestEventData>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if !self.is_released.load(std::sync::atomic::Ordering::Acquire) {
            self.notify.notified().await;
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
    let (gated_b, released, notify) = GatedObserver::new(sub_b.clone());
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
    // on A alone.
    let all_caught_up = event_bus
        .wait_until_all_caught_up(tokio::time::Duration::from_millis(300))
        .await
        .expect("wait_until_all_caught_up failed");
    assert!(
        !all_caught_up,
        "must be false while subscriber B is still blocked on the gate"
    );
    assert_eq!(
        event_bus.subscriber_lag(&sub_a).await.expect("lag A"),
        0,
        "subscriber A should already be caught up even while B is gated"
    );

    // Release B; both must now be reported caught up.
    released.store(true, std::sync::atomic::Ordering::Release);
    notify.notify_waiters();

    let all_caught_up = event_bus
        .wait_until_all_caught_up(tokio::time::Duration::from_secs(3))
        .await
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
