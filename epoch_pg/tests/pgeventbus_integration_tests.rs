mod common;

use epoch_core::prelude::*;
use epoch_derive::EventData;
use epoch_pg::event_bus::PgEventBus;
use epoch_pg::event_store::PgEventStore;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, EventData)]
enum TestEventData {
    TestEvent { value: String },
}

struct TestProjection {
    events: Arc<Mutex<Vec<Event<TestEventData>>>>,
}

impl TestProjection {
    fn new() -> Self {
        Self {
            events: Arc::new(Mutex::new(vec![])),
        }
    }
}

impl Projection<TestEventData> for TestProjection {
    fn apply(
        &mut self,
        event: &Event<TestEventData>,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send>,
    > {
        let events = self.events.clone();
        let event = event.clone();
        Box::pin(async move {
            let mut events = events.lock().await;
            events.push(event);
            Ok(())
        })
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
    let projection_events = projection.events.clone();
    event_bus
        .subscribe(Box::new(projection))
        .await
        .expect("Failed to subscribe projection");

    let stream_id = Uuid::new_v4();
    let event = Event::<TestEventData>::builder()
        .id(Uuid::new_v4())
        .stream_id(stream_id)
        .stream_version(1)
        .event_type("TestEvent".to_string())
        .data(Some(TestEventData::TestEvent {
            value: "test_value".to_string(),
        }))
        .build()
        .unwrap();

    // Give some time for the notification to be processed
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Store the event using PgEventStore, which should trigger the NOTIFY
    event_store
        .store_event(event.clone())
        .await
        .expect("Failed to store event");

    // Give some time for the notification to be processed
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let events_received = projection_events.lock().await;
    assert_eq!(events_received.len(), 1);
    assert_eq!(events_received[0].id, event.id);
    assert_eq!(events_received[0].stream_id, event.stream_id);
    assert_eq!(events_received[0].stream_version, event.stream_version);
    assert_eq!(events_received[0].event_type, event.event_type);
    assert_eq!(events_received[0].data, event.data);

    teardown(&pool).await;
}
