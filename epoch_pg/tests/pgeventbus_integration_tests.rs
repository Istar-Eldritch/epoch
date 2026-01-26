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

impl ProjectionState for TestState {
    fn get_id(&self) -> Uuid {
        Uuid::new_v4()
    }
}

struct TestProjection {
    state_store: InMemoryStateStore<TestState>,
    subscriber_id: String,
}

impl TestProjection {
    pub fn new() -> Self {
        Self::with_subscriber_id(format!("projection:test:{}", Uuid::new_v4()))
    }

    pub fn with_subscriber_id(subscriber_id: String) -> Self {
        TestProjection {
            state_store: InMemoryStateStore::new(),
            subscriber_id,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TestProjectionError {}

#[async_trait]
impl Projection<TestEventData> for TestProjection {
    type State = TestState;
    type StateStore = InMemoryStateStore<Self::State>;
    type EventType = TestEventData;
    type ProjectionError = TestProjectionError;

    fn subscriber_id(&self) -> &str {
        &self.subscriber_id
    }

    fn get_state_store(&self) -> Self::StateStore {
        self.state_store.clone()
    }
    fn apply(
        &self,
        state: Option<Self::State>,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::ProjectionError> {
        if let Some(mut state) = state {
            state.0.push(event.clone());
            Ok(Some(state))
        } else {
            Ok(Some(TestState(vec![event.clone()])))
        }
    }
}

async fn setup() -> (
    PgPool,
    PgEventBus<TestEventData>,
    PgEventStore<PgEventBus<TestEventData>>,
) {
    let _ = env_logger::builder().is_test(true).try_init();
    let pool = common::get_pg_pool().await;

    // Run migrations to set up the schema
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

    // Clean up any malformed events from previous test runs that would break deserialization
    // These events have invalid JSON data that can't be deserialized to TestEventData
    let _ = sqlx::query(
        r#"
        DELETE FROM epoch_events 
        WHERE data IS NOT NULL 
        AND data->>'invalid_field' IS NOT NULL
        "#,
    )
    .execute(&pool)
    .await;

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

    (pool, event_bus, event_store)
}

#[tokio::test]
#[serial]
async fn test_setup_with_migrations() {
    let (pool, _event_bus, _event_store) = setup().await;
    // If setup completes without panicking, migrations and event bus setup was successful
}

#[tokio::test]
#[serial]
async fn test_subscribe_and_event_propagation() {
    let (pool, event_bus, event_store) = setup().await;

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
    let (pool, event_bus, _event_store) = setup().await;

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
    let (pool, event_bus, _event_store) = setup().await;

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
    let (pool, event_bus, event_store) = setup().await;

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
    let (pool, event_bus, event_store) = setup().await;

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
    let (pool, event_bus, _event_store) = setup().await;

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
    let (pool, event_bus, _event_store) = setup().await;

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
    let (pool, event_bus, _event_store) = setup().await;

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
    let (pool, event_bus, _event_store) = setup().await;

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
    let (_pool, event_bus, event_store) = setup().await;

    let projection = TestProjection::new();
    let subscriber_id = Projection::subscriber_id(&projection).to_string();

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
    let (_pool, event_bus, event_store) = setup().await;

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
    let (_pool, event_bus, event_store) = setup().await;

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
    let (pool, event_bus, event_store) = setup().await;

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
    let (pool, event_bus, event_store) = setup().await;

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
    let (pool, event_bus, event_store) = setup().await;

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
    let (pool, _event_bus, _event_store) = setup().await;

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
    let (_pool, event_bus, _event_store) = setup().await;

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
    let (_pool, event_bus, _event_store) = setup().await;

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
    let (_pool, event_bus, _event_store) = setup().await;

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
    let (_pool, event_bus, _event_store) = setup().await;

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
    let (pool, event_bus, event_store) = setup().await;

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
