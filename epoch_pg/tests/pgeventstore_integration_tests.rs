mod common;

use epoch_core::event::{Event, EventData};
use epoch_core::prelude::EventStoreBackend;
use epoch_derive::EventData;
use epoch_mem::InMemoryEventBus;
use epoch_pg::Migrator;
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

    // Run migrations to set up the schema
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

    let event_bus = InMemoryEventBus::new();
    let event_store = PgEventStore::new(pool.clone(), event_bus);
    (pool, event_store)
}

#[tokio::test]
#[serial]
async fn test_store_event() {
    let (_pool, event_store) = setup().await;

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
}

#[tokio::test]
#[serial]
async fn test_read_events() {
    let (_pool, event_store) = setup().await;

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
}

#[tokio::test]
#[serial]
async fn test_read_events_since() {
    let (_pool, event_store) = setup().await;

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
}

#[tokio::test]
#[serial]
async fn test_migrations_create_global_sequence_column() {
    let (pool, _event_store) = setup().await;

    // Verify that the global_sequence column exists
    let result: (i64,) = sqlx::query_as(
        r#"
        SELECT COUNT(*) 
        FROM information_schema.columns 
        WHERE table_name = 'epoch_events' AND column_name = 'global_sequence'
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("Failed to query information_schema");

    assert_eq!(result.0, 1, "global_sequence column should exist");

    // Verify that the sequence exists
    let sequence_result: (i64,) = sqlx::query_as(
        r#"
        SELECT COUNT(*) 
        FROM pg_sequences 
        WHERE schemaname = 'public' AND sequencename = 'epoch_events_global_sequence_seq'
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("Failed to query pg_sequences");

    assert_eq!(
        sequence_result.0, 1,
        "epoch_events_global_sequence_seq sequence should exist"
    );

    // Verify that the index exists
    let index_result: (i64,) = sqlx::query_as(
        r#"
        SELECT COUNT(*) 
        FROM pg_indexes 
        WHERE tablename = 'epoch_events' AND indexname = 'idx_epoch_events_global_sequence'
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("Failed to query pg_indexes");

    assert_eq!(
        index_result.0, 1,
        "idx_epoch_events_global_sequence index should exist"
    );
}

#[tokio::test]
#[serial]
async fn test_migrator_is_idempotent() {
    let (pool, _event_store) = setup().await;

    // Run migrations again - should not error and return 0 applied
    let applied = Migrator::new(pool.clone())
        .run()
        .await
        .expect("Second run should succeed");

    assert_eq!(applied, 0, "No new migrations should be applied");

    // Run a third time for good measure
    let applied = Migrator::new(pool.clone())
        .run()
        .await
        .expect("Third run should succeed");

    assert_eq!(applied, 0, "No new migrations should be applied");
}

#[tokio::test]
#[serial]
async fn test_store_event_assigns_global_sequence() {
    let (_pool, event_store) = setup().await;

    let stream_id = Uuid::new_v4();
    let event = Event::<TestEventData>::builder()
        .id(Uuid::new_v4())
        .stream_id(stream_id)
        .stream_version(1)
        .event_type("TestEvent".to_string())
        .data(Some(TestEventData::TestEvent {
            value: "test".to_string(),
        }))
        .build()
        .unwrap();

    // Before storing, global_sequence should be None
    assert_eq!(event.global_sequence, None);

    event_store.store_event(event.clone()).await.unwrap();

    // Read back and verify global_sequence is assigned
    let mut events = event_store.read_events(stream_id).await.unwrap();
    let read_event = events.next().await.unwrap().unwrap();

    assert!(
        read_event.global_sequence.is_some(),
        "global_sequence should be assigned after storing"
    );
}

#[tokio::test]
#[serial]
async fn test_global_sequence_is_monotonically_increasing() {
    let (_pool, event_store) = setup().await;

    let stream_id1 = Uuid::new_v4();
    let stream_id2 = Uuid::new_v4();

    // Store 3 events across 2 different streams
    let event1 = Event::<TestEventData>::builder()
        .stream_id(stream_id1)
        .stream_version(1)
        .event_type("TestEvent".to_string())
        .data(Some(TestEventData::TestEvent {
            value: "test1".to_string(),
        }))
        .build()
        .unwrap();

    let event2 = Event::<TestEventData>::builder()
        .stream_id(stream_id2)
        .stream_version(1)
        .event_type("TestEvent".to_string())
        .data(Some(TestEventData::TestEvent {
            value: "test2".to_string(),
        }))
        .build()
        .unwrap();

    let event3 = Event::<TestEventData>::builder()
        .stream_id(stream_id1)
        .stream_version(2)
        .event_type("TestEvent".to_string())
        .data(Some(TestEventData::TestEvent {
            value: "test3".to_string(),
        }))
        .build()
        .unwrap();

    event_store.store_event(event1.clone()).await.unwrap();
    event_store.store_event(event2.clone()).await.unwrap();
    event_store.store_event(event3.clone()).await.unwrap();

    // Read all events from both streams
    let mut events1 = event_store.read_events(stream_id1).await.unwrap();
    let read_event1 = events1.next().await.unwrap().unwrap();
    let read_event3 = events1.next().await.unwrap().unwrap();

    let mut events2 = event_store.read_events(stream_id2).await.unwrap();
    let read_event2 = events2.next().await.unwrap().unwrap();

    let gs1 = read_event1
        .global_sequence
        .expect("should have global_sequence");
    let gs2 = read_event2
        .global_sequence
        .expect("should have global_sequence");
    let gs3 = read_event3
        .global_sequence
        .expect("should have global_sequence");

    // Verify global_sequence values are strictly increasing across all events
    assert!(gs1 < gs2, "gs1 ({}) should be less than gs2 ({})", gs1, gs2);
    assert!(gs2 < gs3, "gs2 ({}) should be less than gs3 ({})", gs2, gs3);
}

#[tokio::test]
#[serial]
async fn test_read_events_includes_global_sequence() {
    let (_pool, event_store) = setup().await;

    let stream_id = Uuid::new_v4();
    let event = Event::<TestEventData>::builder()
        .stream_id(stream_id)
        .stream_version(1)
        .event_type("TestEvent".to_string())
        .data(Some(TestEventData::TestEvent {
            value: "test".to_string(),
        }))
        .build()
        .unwrap();

    event_store.store_event(event.clone()).await.unwrap();

    // Read back and verify global_sequence is populated
    let mut events = event_store.read_events(stream_id).await.unwrap();
    let read_event = events.next().await.unwrap().unwrap();

    assert!(
        read_event.global_sequence.is_some(),
        "global_sequence should be populated when reading events"
    );
}

#[tokio::test]
#[serial]
async fn test_store_events_batch() {
    let (_pool, event_store) = setup().await;

    let stream_id = Uuid::new_v4();

    let events: Vec<Event<TestEventData>> = (1..=5)
        .map(|i| {
            Event::<TestEventData>::builder()
                .stream_id(stream_id)
                .stream_version(i)
                .event_type("TestEvent".to_string())
                .data(Some(TestEventData::TestEvent {
                    value: format!("test_{}", i),
                }))
                .build()
                .unwrap()
        })
        .collect();

    // Store all events in a single batch
    event_store.store_events(events).await.unwrap();

    // Read back and verify all events were stored
    let mut read_events = event_store.read_events(stream_id).await.unwrap();
    let mut count = 0;
    while let Some(event) = read_events.next().await {
        let event = event.unwrap();
        count += 1;
        assert!(event.global_sequence.is_some());
    }
    assert_eq!(count, 5);
}

#[tokio::test]
#[serial]
async fn test_store_events_empty_batch() {
    let (_pool, event_store) = setup().await;

    // Empty batch should succeed without error
    event_store.store_events(Vec::new()).await.unwrap();
}

#[tokio::test]
#[serial]
async fn test_store_events_global_sequence_is_sequential() {
    let (_pool, event_store) = setup().await;

    let stream_id = Uuid::new_v4();

    let events: Vec<Event<TestEventData>> = (1..=3)
        .map(|i| {
            Event::<TestEventData>::builder()
                .stream_id(stream_id)
                .stream_version(i)
                .event_type("TestEvent".to_string())
                .data(Some(TestEventData::TestEvent {
                    value: format!("test_{}", i),
                }))
                .build()
                .unwrap()
        })
        .collect();

    event_store.store_events(events).await.unwrap();

    // Read back and verify global_sequence values are sequential
    let mut read_events = event_store.read_events(stream_id).await.unwrap();
    let mut sequences = Vec::new();
    while let Some(event) = read_events.next().await {
        let event = event.unwrap();
        sequences.push(event.global_sequence.unwrap());
    }

    assert_eq!(sequences.len(), 3);
    // Verify sequences are strictly increasing
    for i in 1..sequences.len() {
        assert!(
            sequences[i] > sequences[i - 1],
            "Global sequences should be strictly increasing"
        );
    }
}

#[tokio::test]
#[serial]
async fn test_store_events_in_tx_with_state() {
    let (pool, event_store) = setup().await;

    let stream_id = Uuid::new_v4();

    let events: Vec<Event<TestEventData>> = (1..=3)
        .map(|i| {
            Event::<TestEventData>::builder()
                .stream_id(stream_id)
                .stream_version(i)
                .event_type("TestEvent".to_string())
                .data(Some(TestEventData::TestEvent {
                    value: format!("test_{}", i),
                }))
                .build()
                .unwrap()
        })
        .collect();

    // Use a transaction to store events
    let mut tx = pool.begin().await.unwrap();
    let stored_events = event_store
        .store_events_in_tx(&mut tx, events)
        .await
        .unwrap();

    // Verify stored events have global_sequence
    assert_eq!(stored_events.len(), 3);
    for event in &stored_events {
        assert!(event.global_sequence.is_some());
    }

    // Before commit, events should not be visible from another connection
    let mut read_before_commit = event_store.read_events(stream_id).await.unwrap();
    assert!(read_before_commit.next().await.is_none());

    // Commit the transaction
    tx.commit().await.unwrap();

    // After commit, events should be visible
    let mut read_after_commit = event_store.read_events(stream_id).await.unwrap();
    let mut count = 0;
    while (read_after_commit.next().await).is_some() {
        count += 1;
    }
    assert_eq!(count, 3);

    // Now publish events (normally done after commit)
    event_store.publish_events(stored_events).await.unwrap();
}

#[tokio::test]
#[serial]
async fn test_store_events_in_tx_rollback() {
    let (pool, event_store) = setup().await;

    let stream_id = Uuid::new_v4();

    let events: Vec<Event<TestEventData>> = (1..=3)
        .map(|i| {
            Event::<TestEventData>::builder()
                .stream_id(stream_id)
                .stream_version(i)
                .event_type("TestEvent".to_string())
                .data(Some(TestEventData::TestEvent {
                    value: format!("test_{}", i),
                }))
                .build()
                .unwrap()
        })
        .collect();

    // Use a transaction to store events
    let mut tx = pool.begin().await.unwrap();
    let _stored_events = event_store
        .store_events_in_tx(&mut tx, events)
        .await
        .unwrap();

    // Rollback instead of commit
    tx.rollback().await.unwrap();

    // Events should not be visible
    let mut read_events = event_store.read_events(stream_id).await.unwrap();
    assert!(read_events.next().await.is_none());
}

#[tokio::test]
#[serial]
async fn test_read_events_by_correlation_id() {
    let (_pool, event_store) = setup().await;

    let correlation_id = Uuid::new_v4();
    let stream_a = Uuid::new_v4();
    let stream_b = Uuid::new_v4();

    let event1 = Event::<TestEventData>::builder()
        .stream_id(stream_a)
        .stream_version(1)
        .event_type("TestEvent".to_string())
        .data(Some(TestEventData::TestEvent {
            value: "a1".to_string(),
        }))
        .correlation_id(correlation_id)
        .build()
        .unwrap();

    let event2 = Event::<TestEventData>::builder()
        .stream_id(stream_b)
        .stream_version(1)
        .event_type("TestEvent".to_string())
        .data(Some(TestEventData::TestEvent {
            value: "b1".to_string(),
        }))
        .correlation_id(correlation_id)
        .causation_id(event1.id)
        .build()
        .unwrap();

    event_store.store_event(event1.clone()).await.unwrap();
    event_store.store_event(event2.clone()).await.unwrap();

    let events = event_store
        .read_events_by_correlation_id(correlation_id)
        .await
        .unwrap();

    assert_eq!(events.len(), 2);
    assert_eq!(events[0].id, event1.id);
    assert_eq!(events[1].id, event2.id);
    assert_eq!(events[0].correlation_id, Some(correlation_id));
    assert_eq!(events[1].causation_id, Some(event1.id));
}

#[tokio::test]
#[serial]
async fn test_read_events_by_correlation_id_empty() {
    let (_pool, event_store) = setup().await;

    let events = event_store
        .read_events_by_correlation_id(Uuid::new_v4())
        .await
        .unwrap();

    assert!(events.is_empty());
}

#[tokio::test]
#[serial]
async fn test_read_events_by_correlation_id_ordered_by_global_sequence() {
    let (_pool, event_store) = setup().await;

    let correlation_id = Uuid::new_v4();
    let stream_a = Uuid::new_v4();
    let stream_b = Uuid::new_v4();
    let stream_c = Uuid::new_v4();

    // Store events across 3 streams with the same correlation
    let event1 = Event::<TestEventData>::builder()
        .stream_id(stream_a)
        .stream_version(1)
        .event_type("TestEvent".to_string())
        .data(Some(TestEventData::TestEvent {
            value: "first".to_string(),
        }))
        .correlation_id(correlation_id)
        .build()
        .unwrap();

    let event2 = Event::<TestEventData>::builder()
        .stream_id(stream_b)
        .stream_version(1)
        .event_type("TestEvent".to_string())
        .data(Some(TestEventData::TestEvent {
            value: "second".to_string(),
        }))
        .correlation_id(correlation_id)
        .causation_id(event1.id)
        .build()
        .unwrap();

    let event3 = Event::<TestEventData>::builder()
        .stream_id(stream_c)
        .stream_version(1)
        .event_type("TestEvent".to_string())
        .data(Some(TestEventData::TestEvent {
            value: "third".to_string(),
        }))
        .correlation_id(correlation_id)
        .causation_id(event2.id)
        .build()
        .unwrap();

    event_store.store_event(event1.clone()).await.unwrap();
    event_store.store_event(event2.clone()).await.unwrap();
    event_store.store_event(event3.clone()).await.unwrap();

    let events = event_store
        .read_events_by_correlation_id(correlation_id)
        .await
        .unwrap();

    assert_eq!(events.len(), 3);
    // Verify ordering by global_sequence
    let gs1 = events[0].global_sequence.unwrap();
    let gs2 = events[1].global_sequence.unwrap();
    let gs3 = events[2].global_sequence.unwrap();
    assert!(gs1 < gs2, "gs1 ({}) < gs2 ({})", gs1, gs2);
    assert!(gs2 < gs3, "gs2 ({}) < gs3 ({})", gs2, gs3);
}

#[tokio::test]
#[serial]
async fn test_trace_causation_chain() {
    let (_pool, event_store) = setup().await;

    let correlation_id = Uuid::new_v4();
    let stream_a = Uuid::new_v4();
    let stream_b = Uuid::new_v4();
    let stream_c = Uuid::new_v4();

    // A → B → C chain
    let event_a = Event::<TestEventData>::builder()
        .stream_id(stream_a)
        .stream_version(1)
        .event_type("TestEvent".to_string())
        .data(Some(TestEventData::TestEvent {
            value: "root".to_string(),
        }))
        .correlation_id(correlation_id)
        .build()
        .unwrap();

    let event_b = Event::<TestEventData>::builder()
        .stream_id(stream_b)
        .stream_version(1)
        .event_type("TestEvent".to_string())
        .data(Some(TestEventData::TestEvent {
            value: "middle".to_string(),
        }))
        .correlation_id(correlation_id)
        .causation_id(event_a.id)
        .build()
        .unwrap();

    let event_c = Event::<TestEventData>::builder()
        .stream_id(stream_c)
        .stream_version(1)
        .event_type("TestEvent".to_string())
        .data(Some(TestEventData::TestEvent {
            value: "leaf".to_string(),
        }))
        .correlation_id(correlation_id)
        .causation_id(event_b.id)
        .build()
        .unwrap();

    event_store.store_event(event_a.clone()).await.unwrap();
    event_store.store_event(event_b.clone()).await.unwrap();
    event_store.store_event(event_c.clone()).await.unwrap();

    // Trace from middle → should include all 3
    let chain = event_store.trace_causation_chain(event_b.id).await.unwrap();

    assert_eq!(chain.len(), 3);
    assert_eq!(chain[0].id, event_a.id);
    assert_eq!(chain[1].id, event_b.id);
    assert_eq!(chain[2].id, event_c.id);
}

#[tokio::test]
#[serial]
async fn test_trace_causation_chain_not_found() {
    let (_pool, event_store) = setup().await;

    let chain = event_store
        .trace_causation_chain(Uuid::new_v4())
        .await
        .unwrap();

    assert!(chain.is_empty());
}

#[tokio::test]
#[serial]
async fn test_trace_causation_chain_excludes_sibling() {
    let (_pool, event_store) = setup().await;

    let correlation_id = Uuid::new_v4();
    let stream_a = Uuid::new_v4();
    let stream_b = Uuid::new_v4();
    let stream_c = Uuid::new_v4();

    // A → B, A → C (branching)
    let event_a = Event::<TestEventData>::builder()
        .stream_id(stream_a)
        .stream_version(1)
        .event_type("TestEvent".to_string())
        .data(Some(TestEventData::TestEvent {
            value: "root".to_string(),
        }))
        .correlation_id(correlation_id)
        .build()
        .unwrap();

    let event_b = Event::<TestEventData>::builder()
        .stream_id(stream_b)
        .stream_version(1)
        .event_type("TestEvent".to_string())
        .data(Some(TestEventData::TestEvent {
            value: "branch_b".to_string(),
        }))
        .correlation_id(correlation_id)
        .causation_id(event_a.id)
        .build()
        .unwrap();

    let event_c = Event::<TestEventData>::builder()
        .stream_id(stream_c)
        .stream_version(1)
        .event_type("TestEvent".to_string())
        .data(Some(TestEventData::TestEvent {
            value: "branch_c".to_string(),
        }))
        .correlation_id(correlation_id)
        .causation_id(event_a.id)
        .build()
        .unwrap();

    event_store.store_event(event_a.clone()).await.unwrap();
    event_store.store_event(event_b.clone()).await.unwrap();
    event_store.store_event(event_c.clone()).await.unwrap();

    // Trace from B → should include A and B, but not C
    let chain = event_store.trace_causation_chain(event_b.id).await.unwrap();

    assert_eq!(chain.len(), 2);
    assert_eq!(chain[0].id, event_a.id);
    assert_eq!(chain[1].id, event_b.id);
}
