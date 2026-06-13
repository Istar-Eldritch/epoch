mod common;

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

impl Projection<TestEventData> for TestProjection {}

async fn setup() -> Option<(
    PgPool,
    PgEventBus<TestEventData>,
    PgEventStore<PgEventBus<TestEventData>>,
)> {
    let _ = env_logger::builder().is_test(true).try_init();
    let pool = common::try_get_pg_pool().await?;

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
    let _ = env_logger::builder().is_test(true).try_init();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };

    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

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
    let _ = env_logger::builder().is_test(true).try_init();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };

    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

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
    let _ = env_logger::builder().is_test(true).try_init();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };

    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

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
    let _ = env_logger::builder().is_test(true).try_init();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };

    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

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
    let _ = env_logger::builder().is_test(true).try_init();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };

    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

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
    let _ = env_logger::builder().is_test(true).try_init();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };

    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

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
    let _ = env_logger::builder().is_test(true).try_init();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };

    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

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
    let _ = env_logger::builder().is_test(true).try_init();
    let pool = common::try_get_pg_pool().await?;

    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

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

    let _ = env_logger::builder().is_test(true).try_init();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };

    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

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
    let _ = env_logger::builder().is_test(true).try_init();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };

    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

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
    let _ = env_logger::builder().is_test(true).try_init();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };

    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

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
    config: epoch_pg::event_bus::ReliableDeliveryConfig,
) -> (
    PgEventBus<TestEventData>,
    String,
    InMemoryStateStore<TestState>,
) {
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
    let _ = env_logger::builder().is_test(true).try_init();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

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
    let _ = env_logger::builder().is_test(true).try_init();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

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
    let _ = env_logger::builder().is_test(true).try_init();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

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
    let _ = env_logger::builder().is_test(true).try_init();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

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
    let _ = env_logger::builder().is_test(true).try_init();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

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
    let _ = env_logger::builder().is_test(true).try_init();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

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
    let _ = env_logger::builder().is_test(true).try_init();
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

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
