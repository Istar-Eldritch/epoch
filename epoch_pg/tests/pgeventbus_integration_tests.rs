mod common;

use async_trait::async_trait;
use epoch_core::prelude::*;
use epoch_derive::EventData;
use epoch_mem::InMemoryStateStorage;
use epoch_pg::event_bus::PgEventBus;
use epoch_pg::event_store::PgEventStore;
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, EventData)]
enum TestEventData {
    TestEvent { value: String },
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

struct TestProjection(InMemoryStateStorage<Vec<Event<TestEventData>>>);

impl TestProjection {
    pub fn new() -> Self {
        TestProjection(InMemoryStateStorage::new())
    }
}

#[async_trait]
impl Projection<TestEventData> for TestProjection {
    type State = Vec<Event<TestEventData>>;
    type StateStorage = InMemoryStateStorage<Self::State>;
    type CreateEvent = TestEventData;
    type UpdateEvent = TestEventData;
    type DeleteEvent = TestEventData;
    fn get_storage(&self) -> Self::StateStorage {
        self.0.clone()
    }
    fn apply_create(
        &self,
        event: &Event<Self::CreateEvent>,
    ) -> Result<Self::State, Box<dyn std::error::Error + Send + Sync>> {
        Ok(vec![event.clone()])
    }
    fn apply_update(
        &self,
        mut state: Self::State,
        event: &Event<Self::CreateEvent>,
    ) -> Result<Self::State, Box<dyn std::error::Error + Send + Sync>> {
        state.push(event.clone());
        Ok(state)
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
async fn test_initialize() {
    let (pool, _event_bus, _event_store) = setup().await;
    // If setup completes without panicking, initialization was successful
    teardown(&pool).await;
}

#[tokio::test]
async fn test_subscribe_and_event_propagation() {
    let (pool, event_bus, event_store) = setup().await;

    let projection = TestProjection::new();
    let projection_events = projection.get_storage().clone();
    event_bus
        .subscribe(projection)
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
    assert_eq!(events_received.len(), 1);
    assert_eq!(events_received[0].id, event.id);
    assert_eq!(events_received[0].stream_id, event.stream_id);
    assert_eq!(events_received[0].stream_version, event.stream_version);
    assert_eq!(events_received[0].event_type, event.event_type);
    assert_eq!(events_received[0].data, event.data);

    teardown(&pool).await;
}
