mod common;

use epoch_core::event::{Event, EventData};
use epoch_core::prelude::EventStoreBackend;
use epoch_derive::EventData;
use epoch_mem::InMemoryEventBus;
use epoch_pg::event_store::PgEventStore;
use serde::{Deserialize, Serialize};
use serial_test::serial;
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, EventData)]
enum TestEventData {
    TestEvent { value: String },
}

use futures_util::StreamExt;

async fn setup() -> (PgPool, PgEventStore<InMemoryEventBus<TestEventData>>) {
    let pool = common::get_pg_pool().await;
    let event_bus = InMemoryEventBus::new();
    let event_store = PgEventStore::new(pool.clone(), event_bus);
    event_store
        .initialize()
        .await
        .expect("Failed to initialize event store");
    (pool, event_store)
}

async fn teardown(pool: &PgPool) {
    sqlx::query("DROP TABLE IF EXISTS events CASCADE")
        .execute(pool)
        .await
        .expect("Failed to drop table");
}

#[tokio::test]
#[serial]
async fn test_store_event() {
    let (pool, event_store) = setup().await;

    let event = Event::<TestEventData>::builder()
        .id(Uuid::new_v4())
        .stream_id(Uuid::new_v4())
        .stream_version(1)
        .event_type("TestEvent".to_string())
        .data(Some(TestEventData::TestEvent {
            value: "test".to_string(),
        }))
        .build()
        .unwrap();
    event_store.store_event(event.clone()).await.unwrap();

    let event_overflow = Event::<TestEventData>::builder()
        .id(Uuid::new_v4())
        .stream_id(Uuid::new_v4())
        .stream_version(u64::MAX)
        .event_type("TestEvent".to_string())
        .data(Some(TestEventData::TestEvent {
            value: "test_overflow".to_string(),
        }))
        .build()
        .unwrap();
    assert!(matches!(
        event_store.store_event(event_overflow.clone()).await,
        Err(epoch_pg::event_store::PgEventStoreError::DBError(
            sqlx::error::Error::InvalidArgument(_)
        ))
    ));
    teardown(&pool).await;
}

#[tokio::test]
#[serial]
async fn test_read_events() {
    let (pool, event_store) = setup().await;

    let stream_id = Uuid::new_v4();

    let event1 = Event::<TestEventData>::builder()
        .id(Uuid::new_v4())
        .stream_id(stream_id)
        .stream_version(1)
        .event_type("TestEvent1".to_string())
        .data(Some(TestEventData::TestEvent {
            value: "test1".to_string(),
        }))
        .build()
        .unwrap();

    let event2 = Event::<TestEventData>::builder()
        .id(Uuid::new_v4())
        .stream_id(stream_id)
        .stream_version(2)
        .event_type("TestEvent2".to_string())
        .data(Some(TestEventData::TestEvent {
            value: "test2".to_string(),
        }))
        .build()
        .unwrap();

    event_store.store_event(event1.clone()).await.unwrap();
    event_store.store_event(event2.clone()).await.unwrap();

    let mut events = event_store.read_events(stream_id).await.unwrap();

    let read_event1 = events.next().await.unwrap().unwrap();
    assert_eq!(read_event1.id, event1.id);
    assert_eq!(read_event1.stream_id, event1.stream_id);
    assert_eq!(read_event1.stream_version, event1.stream_version);
    assert_eq!(read_event1.event_type, event1.event_type);
    assert_eq!(read_event1.data, event1.data);

    let read_event2 = events.next().await.unwrap().unwrap();
    assert_eq!(read_event2.id, event2.id);
    assert_eq!(read_event2.stream_id, event2.stream_id);
    assert_eq!(read_event2.stream_version, event2.stream_version);
    assert_eq!(read_event2.event_type, event2.event_type);
    assert_eq!(read_event2.data, event2.data);

    assert!(events.next().await.is_none());

    teardown(&pool).await;
}

#[tokio::test]
#[serial]
async fn test_read_events_since() {
    let (pool, event_store) = setup().await;

    let stream_id = Uuid::new_v4();

    let event1 = Event::<TestEventData>::builder()
        .id(Uuid::new_v4())
        .stream_id(stream_id)
        .stream_version(1)
        .event_type("TestEvent1".to_string())
        .data(Some(TestEventData::TestEvent {
            value: "test1".to_string(),
        }))
        .build()
        .unwrap();

    let event2 = Event::<TestEventData>::builder()
        .id(Uuid::new_v4())
        .stream_id(stream_id)
        .stream_version(2)
        .event_type("TestEvent2".to_string())
        .data(Some(TestEventData::TestEvent {
            value: "test2".to_string(),
        }))
        .build()
        .unwrap();

    let event3 = Event::<TestEventData>::builder()
        .id(Uuid::new_v4())
        .stream_id(stream_id)
        .stream_version(3)
        .event_type("TestEvent3".to_string())
        .data(Some(TestEventData::TestEvent {
            value: "test3".to_string(),
        }))
        .build()
        .unwrap();

    event_store.store_event(event1.clone()).await.unwrap();
    event_store.store_event(event2.clone()).await.unwrap();
    event_store.store_event(event3.clone()).await.unwrap();

    // Read events starting from version 2
    let mut events = event_store.read_events_since(stream_id, 2).await.unwrap();

    let read_event2 = events.next().await.unwrap().unwrap();
    assert_eq!(read_event2.id, event2.id);
    assert_eq!(read_event2.stream_id, event2.stream_id);
    assert_eq!(read_event2.stream_version, event2.stream_version);
    assert_eq!(read_event2.event_type, event2.event_type);
    assert_eq!(read_event2.data, event2.data);

    let read_event3 = events.next().await.unwrap().unwrap();
    assert_eq!(read_event3.id, event3.id);
    assert_eq!(read_event3.stream_id, event3.stream_id);
    assert_eq!(read_event3.stream_version, event3.stream_version);
    assert_eq!(read_event3.event_type, event3.event_type);
    assert_eq!(read_event3.data, event3.data);

    assert!(events.next().await.is_none());

    teardown(&pool).await;
}
