mod common;

use epoch_core::prelude::*;
use epoch_derive::EventData;
use epoch_mem::InMemoryStateStore;
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
struct TestState {
    id: Uuid,
    events: Vec<Event<TestEventData>>,
    version: u64,
}

impl TestState {
    fn new(id: Uuid) -> Self {
        Self {
            id,
            events: Vec::new(),
            version: 0,
        }
    }
}

impl EventApplicatorState for TestState {
    fn get_id(&self) -> Uuid {
        self.id
    }
}

impl ProjectionState for TestState {
    fn get_version(&self) -> u64 {
        self.version
    }

    fn set_version(&mut self, version: u64) {
        self.version = version;
    }
}

struct TestProjection {
    state_store: InMemoryStateStore<TestState>,
    event_store: PgEventStore<PgEventBus<TestEventData>>,
}

impl TestProjection {
    pub fn new(event_store: PgEventStore<PgEventBus<TestEventData>>) -> Self {
        TestProjection {
            state_store: InMemoryStateStore::new(),
            event_store,
        }
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
            state.events.push(event.clone());
            Ok(Some(state))
        } else {
            let mut state = TestState::new(event.stream_id);
            state.events.push(event.clone());
            Ok(Some(state))
        }
    }
}

impl Projection<TestEventData> for TestProjection {
    type EventStore = PgEventStore<PgEventBus<TestEventData>>;

    fn get_event_store(&self) -> Self::EventStore {
        self.event_store.clone()
    }
}

async fn setup() -> (
    PgPool,
    PgEventBus<TestEventData>,
    PgEventStore<PgEventBus<TestEventData>>,
) {
    let _ = env_logger::builder().is_test(true).try_init();
    let pool = common::get_pg_pool().await;
    let channel_name = format!("test_channel_{}", Uuid::new_v4().simple());
    let event_bus = PgEventBus::new(pool.clone(), channel_name);
    let event_store = PgEventStore::new(pool.clone(), event_bus.clone());

    event_store
        .initialize()
        .await
        .expect("Failed to initialize event store");
    event_bus
        .initialize()
        .await
        .expect("Failed to initialize event bus");

    (pool, event_bus, event_store)
}

async fn teardown(pool: &PgPool) {
    sqlx::query("DROP TABLE IF EXISTS events CASCADE")
        .execute(pool)
        .await
        .expect("Failed to drop table");
}

#[tokio::test]
#[serial]
async fn test_initialize() {
    let (pool, _event_bus, _event_store) = setup().await;
    // If setup completes without panicking, initialization was successful
    teardown(&pool).await;
}

#[tokio::test]
#[serial]
async fn test_subscribe_and_event_propagation() {
    let (pool, event_bus, event_store) = setup().await;

    let projection = TestProjection::new(event_store.clone());
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
    assert_eq!(events_received.events.len(), 1);
    assert_eq!(events_received.events[0].id, event.id);
    assert_eq!(events_received.events[0].stream_id, event.stream_id);
    assert_eq!(
        events_received.events[0].stream_version,
        event.stream_version
    );
    assert_eq!(events_received.events[0].event_type, event.event_type);
    assert_eq!(events_received.events[0].data, event.data);

    teardown(&pool).await;
}

#[tokio::test]
#[serial]
async fn test_noop_publish() {
    let (pool, event_bus, event_store) = setup().await;

    let projection = TestProjection::new(event_store.clone());
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
    let db_events: Vec<PgDBEvent> = sqlx::query_as("SELECT * FROM events WHERE id = $1")
        .bind(event.id)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(db_events.is_empty());

    teardown(&pool).await;
}

/// Tests that malformed events in the event store cause re-hydration to fail.
///
/// With the re-hydration approach to handle race conditions, projections read from
/// the event store to catch up on any missed events. If there are malformed events
/// in the store, re-hydration will fail for subsequent events on the same stream.
///
/// This test verifies that:
/// 1. The first valid event is processed correctly
/// 2. When a malformed event exists in the history, subsequent events on that stream
///    fail to process (because re-hydration encounters the malformed event)
#[tokio::test]
#[serial]
async fn test_event_data_deserialization_failure() {
    let (pool, event_bus, event_store) = setup().await;

    let projection = TestProjection::new(event_store.clone());
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
        INSERT INTO events (id, stream_id, stream_version, event_type, data, created_at, actor_id, purger_id, purged_at)
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

    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // First event should be processed successfully
    let events_received = projection_events
        .get_state(stream_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(events_received.events.len(), 1);
    assert!(
        events_received
            .events
            .iter()
            .any(|e| e.id == valid_event.id)
    );

    // Malformed event data (e.g., missing a required field, or wrong type)
    let malformed_event_id = Uuid::new_v4();
    let malformed_event_data = serde_json::json!({ "invalid_field": 123 }); // Malformed data
    sqlx::query(
        r#"
        INSERT INTO events (id, stream_id, stream_version, event_type, data, created_at)
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

    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // The malformed event itself won't be applied (deserialization fails before apply)
    // State should still have only 1 event
    let events_received = projection_events
        .get_state(stream_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(events_received.events.len(), 1);

    // Another valid event - this will fail during re-hydration because it needs
    // to read through the malformed event to catch up
    let another_valid_event = new_event(stream_id, 3, "another_valid_data");
    sqlx::query(
        r#"
        INSERT INTO events (id, stream_id, stream_version, event_type, data, created_at, actor_id, purger_id, purged_at)
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

    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    let events_received = projection_events
        .get_state(stream_id)
        .await
        .unwrap()
        .unwrap();

    // Only the first valid event should be in state - the third event couldn't be processed
    // because re-hydration failed when it encountered the malformed event at version 2
    assert_eq!(events_received.events.len(), 1);
    assert!(
        events_received
            .events
            .iter()
            .any(|e| e.id == valid_event.id)
    );
    assert!(
        !events_received
            .events
            .iter()
            .any(|e| e.id == malformed_event_id)
    );
    assert!(
        !events_received
            .events
            .iter()
            .any(|e| e.id == another_valid_event.id)
    );

    teardown(&pool).await;
}

#[tokio::test]
#[serial]
async fn test_multiple_subscribers() {
    let (pool, event_bus, event_store) = setup().await;

    let projection1 = TestProjection::new(event_store.clone());
    let projection_events1 = projection1.get_state_store().clone();
    event_bus
        .subscribe(ProjectionHandler::new(projection1))
        .await
        .expect("Failed to subscribe projection 1");

    let projection2 = TestProjection::new(event_store.clone());
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
    assert_eq!(events_received1.events.len(), 1);
    assert_eq!(events_received1.events[0].id, event.id);

    let events_received2 = projection_events2
        .get_state(stream_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(events_received2.events.len(), 1);
    assert_eq!(events_received2.events[0].id, event.id);

    teardown(&pool).await;
}
