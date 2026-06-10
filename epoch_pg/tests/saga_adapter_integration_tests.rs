//! Integration tests for [`SagaAdapter`] subscribing one saga to multiple
//! `PgEventBus` instances carrying different parent event enums.
//!
//! Verifies:
//! * A saga receives events from both its native bus (via `SagaHandler`) and a
//!   foreign bus (via `SagaAdapter`).
//! * Each subscription advances its own checkpoint independently.
//! * Metadata (`id`, `correlation_id`, `causation_id`, `global_sequence`) is
//!   preserved when events flow through the adapter's converter.
//! * After a restart, each subscriber resumes from its own checkpoint and does
//!   not reprocess previously-handled events.
//!
//! Test isolation: every test generates a unique suffix used for both
//! subscriber IDs and stream IDs, so concurrent rows in the shared
//! `epoch_events` table from prior runs do not interfere.

mod common;

use async_trait::async_trait;
use epoch_core::prelude::*;
use epoch_core::saga::{Saga, SagaAdapter, SagaHandler};
use epoch_derive::EventData;
use epoch_mem::InMemoryStateStore;
use epoch_pg::Migrator;
use epoch_pg::event_bus::PgEventBus;
use epoch_pg::event_store::PgEventStore;
use serial_test::serial;
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use uuid::Uuid;

// === Two unrelated parent event enums (different "buses") ===

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, EventData)]
enum SourceEvent {
    SourceTick { saga_id: Uuid, value: i32 },
}

impl TryFrom<&SourceEvent> for SourceEvent {
    type Error = epoch_core::event::EnumConversionError;
    fn try_from(v: &SourceEvent) -> Result<Self, Self::Error> {
        Ok(v.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, EventData)]
enum TargetEvent {
    NativeTick { saga_id: Uuid, value: i32 },
    BridgedTick { saga_id: Uuid, value: i32 },
}

impl TryFrom<&TargetEvent> for TargetEvent {
    type Error = epoch_core::event::EnumConversionError;
    fn try_from(v: &TargetEvent) -> Result<Self, Self::Error> {
        Ok(v.clone())
    }
}

// === Saga over TargetEvent, recording events whose saga_id matches a filter ===

#[derive(Debug, Clone, Default)]
struct CounterState {
    native_count: u32,
    bridged_count: u32,
}

#[derive(Clone)]
struct CounterSaga {
    state_store: InMemoryStateStore<CounterState>,
    subscriber_id: String,
    /// Only events with `saga_id == filter_saga_id` are recorded; everything
    /// else (e.g. leftover events from prior test runs) is ignored.
    filter_saga_id: Uuid,
    handled: Arc<Mutex<Vec<HandledRecord>>>,
}

#[derive(Debug, Clone)]
struct HandledRecord {
    event_id: Uuid,
    data: TargetEvent,
    correlation_id: Option<Uuid>,
    causation_id: Option<Uuid>,
}

impl CounterSaga {
    fn new(subscriber_id: impl Into<String>, filter_saga_id: Uuid) -> Self {
        Self {
            state_store: InMemoryStateStore::new(),
            subscriber_id: subscriber_id.into(),
            filter_saga_id,
            handled: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl epoch_core::SubscriberId for CounterSaga {
    fn subscriber_id(&self) -> &str {
        &self.subscriber_id
    }
}

#[derive(Debug, thiserror::Error)]
enum CounterSagaError {}

#[async_trait]
impl Saga<TargetEvent> for CounterSaga {
    type State = CounterState;
    type StateStore = InMemoryStateStore<CounterState>;
    type SagaError = CounterSagaError;
    type EventType = TargetEvent;

    fn get_state_store(&self) -> Self::StateStore {
        self.state_store.clone()
    }

    fn get_id_from_event(&self, event: &Event<Self::EventType>) -> Uuid {
        match event.data.as_ref().unwrap() {
            TargetEvent::NativeTick { saga_id, .. } | TargetEvent::BridgedTick { saga_id, .. } => {
                *saga_id
            }
        }
    }

    async fn handle_event(
        &self,
        mut state: Self::State,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::SagaError> {
        let data = event.data.as_ref().unwrap().clone();
        let saga_id = match &data {
            TargetEvent::NativeTick { saga_id, .. } | TargetEvent::BridgedTick { saga_id, .. } => {
                *saga_id
            }
        };
        if saga_id != self.filter_saga_id {
            return Ok(None);
        }
        match &data {
            TargetEvent::NativeTick { .. } => state.native_count += 1,
            TargetEvent::BridgedTick { .. } => state.bridged_count += 1,
        }
        self.handled.lock().await.push(HandledRecord {
            event_id: event.id,
            data,
            correlation_id: event.correlation_id,
            causation_id: event.causation_id,
        });
        Ok(Some(state))
    }
}

// === Shared setup: one DB, two buses with distinct channels ===

async fn setup_two_buses() -> Option<(
    PgPool,
    PgEventBus<SourceEvent>,
    PgEventStore<PgEventBus<SourceEvent>>,
    PgEventBus<TargetEvent>,
    PgEventStore<PgEventBus<TargetEvent>>,
)> {
    let _ = env_logger::builder().is_test(true).try_init();
    let pool = common::try_get_pg_pool().await?;
    Migrator::new(pool.clone()).run().await.unwrap();

    let source_channel = format!("src_{}", Uuid::new_v4().simple());
    let target_channel = format!("tgt_{}", Uuid::new_v4().simple());

    let source_bus = PgEventBus::<SourceEvent>::new(pool.clone(), source_channel);
    let target_bus = PgEventBus::<TargetEvent>::new(pool.clone(), target_channel);

    let source_store = PgEventStore::new(pool.clone(), source_bus.clone());
    let target_store = PgEventStore::new(pool.clone(), target_bus.clone());

    source_bus.setup_trigger().await.unwrap();
    target_bus.setup_trigger().await.unwrap();

    Some((pool, source_bus, source_store, target_bus, target_store))
}

fn target_event(stream_id: Uuid, ver: u64, saga_id: Uuid, value: i32) -> Event<TargetEvent> {
    Event::<TargetEvent>::builder()
        .stream_id(stream_id)
        .event_type("NativeTick".into())
        .stream_version(ver)
        .data(Some(TargetEvent::NativeTick { saga_id, value }))
        .build()
        .unwrap()
}

fn source_event(stream_id: Uuid, ver: u64, saga_id: Uuid, value: i32) -> Event<SourceEvent> {
    Event::<SourceEvent>::builder()
        .stream_id(stream_id)
        .event_type("SourceTick".into())
        .stream_version(ver)
        .data(Some(SourceEvent::SourceTick { saga_id, value }))
        .build()
        .unwrap()
}

async fn fetch_checkpoint(pool: &PgPool, subscriber_id: &str) -> Option<u64> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT last_global_sequence FROM epoch_event_bus_checkpoints WHERE subscriber_id = $1",
    )
    .bind(subscriber_id)
    .fetch_optional(pool)
    .await
    .unwrap();
    row.map(|(s,)| s as u64)
}

/// Generates a unique tag for subscriber and stream IDs in a single test,
/// preventing collision with prior test runs that share the same DB.
fn unique_tag() -> String {
    Uuid::new_v4().simple().to_string()
}

// === Tests ===

#[tokio::test]
#[serial]
async fn saga_adapter_receives_events_from_foreign_bus() {
    let Some((_pool, source_bus, source_store, target_bus, target_store)) = setup_two_buses().await
    else {
        return;
    };
    let tag = unique_tag();
    let native_sub_id = format!("saga:counter:{}", tag);
    let foreign_sub_id = format!("saga:counter:{}:source-bus", tag);
    let saga_id = Uuid::new_v4();
    let target_stream = Uuid::new_v4();
    let source_stream = Uuid::new_v4();

    let saga = Arc::new(CounterSaga::new(&native_sub_id, saga_id));

    target_bus
        .subscribe(SagaHandler::new(saga.clone()))
        .await
        .unwrap();
    source_bus
        .subscribe(SagaAdapter::new(
            saga.clone(),
            foreign_sub_id,
            |e: &SourceEvent| match e {
                SourceEvent::SourceTick { saga_id, value } => Some(TargetEvent::BridgedTick {
                    saga_id: *saga_id,
                    value: *value,
                }),
            },
        ))
        .await
        .unwrap();

    target_bus.start_listener().await.unwrap();
    source_bus.start_listener().await.unwrap();

    target_store
        .store_event(target_event(target_stream, 1, saga_id, 10))
        .await
        .unwrap();
    source_store
        .store_event(source_event(source_stream, 1, saga_id, 20))
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(1500)).await;

    let handled = saga.handled.lock().await;
    assert_eq!(
        handled.len(),
        2,
        "saga should see both events, got {:?}",
        handled
    );
    assert!(
        handled
            .iter()
            .any(|r| matches!(r.data, TargetEvent::NativeTick { value: 10, .. })),
        "missing NativeTick"
    );
    assert!(
        handled
            .iter()
            .any(|r| matches!(r.data, TargetEvent::BridgedTick { value: 20, .. })),
        "missing BridgedTick (foreign-bus event via adapter)"
    );
}

#[tokio::test]
#[serial]
async fn saga_adapter_preserves_event_metadata_through_conversion() {
    let Some((_pool, source_bus, source_store, _target_bus, _target_store)) =
        setup_two_buses().await
    else {
        return;
    };
    let tag = unique_tag();
    let foreign_sub_id = format!("saga:meta:{}:source-bus", tag);
    let saga_id = Uuid::new_v4();
    let stream_id = Uuid::new_v4();

    let saga = Arc::new(CounterSaga::new(format!("saga:meta:{}", tag), saga_id));
    source_bus
        .subscribe(SagaAdapter::new(
            saga.clone(),
            foreign_sub_id,
            |e: &SourceEvent| match e {
                SourceEvent::SourceTick { saga_id, value } => Some(TargetEvent::BridgedTick {
                    saga_id: *saga_id,
                    value: *value,
                }),
            },
        ))
        .await
        .unwrap();
    source_bus.start_listener().await.unwrap();

    let event_id = Uuid::new_v4();
    let correlation = Uuid::new_v4();
    let causation = Uuid::new_v4();

    let event = Event::<SourceEvent>::builder()
        .id(event_id)
        .stream_id(stream_id)
        .event_type("SourceTick".into())
        .stream_version(1)
        .correlation_id(correlation)
        .causation_id(causation)
        .data(Some(SourceEvent::SourceTick { saga_id, value: 99 }))
        .build()
        .unwrap();
    source_store.store_event(event).await.unwrap();

    tokio::time::sleep(Duration::from_millis(1500)).await;

    let handled = saga.handled.lock().await;
    assert_eq!(handled.len(), 1, "saga should see exactly one event");
    let r = &handled[0];
    assert_eq!(r.event_id, event_id, "event id preserved through adapter");
    assert_eq!(
        r.correlation_id,
        Some(correlation),
        "correlation_id preserved"
    );
    assert_eq!(r.causation_id, Some(causation), "causation_id preserved");
}

#[tokio::test]
#[serial]
async fn saga_adapter_advances_independent_checkpoints() {
    let Some((pool, source_bus, source_store, target_bus, target_store)) = setup_two_buses().await
    else {
        return;
    };
    let tag = unique_tag();
    let native_sub_id = format!("saga:cp:{}", tag);
    let foreign_sub_id = format!("saga:cp:{}:source-bus", tag);
    let saga_id = Uuid::new_v4();
    let target_stream = Uuid::new_v4();
    let source_stream = Uuid::new_v4();

    let saga = Arc::new(CounterSaga::new(&native_sub_id, saga_id));
    target_bus
        .subscribe(SagaHandler::new(saga.clone()))
        .await
        .unwrap();
    source_bus
        .subscribe(SagaAdapter::new(
            saga.clone(),
            foreign_sub_id.clone(),
            |e: &SourceEvent| match e {
                SourceEvent::SourceTick { saga_id, value } => Some(TargetEvent::BridgedTick {
                    saga_id: *saga_id,
                    value: *value,
                }),
            },
        ))
        .await
        .unwrap();
    target_bus.start_listener().await.unwrap();
    source_bus.start_listener().await.unwrap();

    for i in 0..3u64 {
        target_store
            .store_event(target_event(target_stream, i + 1, saga_id, i as i32))
            .await
            .unwrap();
    }
    for i in 0..2u64 {
        source_store
            .store_event(source_event(source_stream, i + 1, saga_id, 100 + i as i32))
            .await
            .unwrap();
    }

    tokio::time::sleep(Duration::from_millis(2000)).await;

    let cp_native = fetch_checkpoint(&pool, &native_sub_id).await;
    let cp_foreign = fetch_checkpoint(&pool, &foreign_sub_id).await;

    assert!(
        cp_native.is_some() && cp_native.unwrap() > 0,
        "native subscriber checkpoint should have advanced, got {:?}",
        cp_native
    );
    assert!(
        cp_foreign.is_some() && cp_foreign.unwrap() > 0,
        "adapter subscriber checkpoint should have advanced, got {:?}",
        cp_foreign
    );

    let handled = saga.handled.lock().await;
    let native_seen = handled
        .iter()
        .filter(|r| matches!(r.data, TargetEvent::NativeTick { .. }))
        .count();
    let bridged_seen = handled
        .iter()
        .filter(|r| matches!(r.data, TargetEvent::BridgedTick { .. }))
        .count();
    assert_eq!(native_seen, 3, "should see 3 native events");
    assert_eq!(bridged_seen, 2, "should see 2 bridged events");
}

#[tokio::test]
#[serial]
async fn saga_adapter_resumes_from_checkpoint_after_restart() {
    let Some((pool, source_bus, source_store, _t_bus, _t_store)) = setup_two_buses().await else {
        return;
    };
    let tag = unique_tag();
    let foreign_sub_id = format!("saga:restart:{}:source-bus", tag);
    let saga_id = Uuid::new_v4();
    let stream_id = Uuid::new_v4();
    let first_channel = source_bus.channel_name().to_string();

    // First run.
    {
        let saga = Arc::new(CounterSaga::new(format!("saga:restart:{}", tag), saga_id));
        source_bus
            .subscribe(SagaAdapter::new(
                saga.clone(),
                foreign_sub_id.clone(),
                |e: &SourceEvent| match e {
                    SourceEvent::SourceTick { saga_id, value } => Some(TargetEvent::BridgedTick {
                        saga_id: *saga_id,
                        value: *value,
                    }),
                },
            ))
            .await
            .unwrap();
        source_bus.start_listener().await.unwrap();

        source_store
            .store_event(source_event(stream_id, 1, saga_id, 1))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(saga.handled.lock().await.len(), 1, "first run sees 1 event");
        source_bus.shutdown().await.unwrap();
    }

    let cp_after_first = fetch_checkpoint(&pool, &foreign_sub_id)
        .await
        .expect("checkpoint should exist after first run");
    assert!(
        cp_after_first > 0,
        "checkpoint should have advanced past first event"
    );

    // Second run on a fresh bus, same channel, same subscriber id.
    let source_bus2 = PgEventBus::<SourceEvent>::new(pool.clone(), first_channel);
    let source_store2 = PgEventStore::new(pool.clone(), source_bus2.clone());
    source_bus2.setup_trigger().await.unwrap();

    let saga2 = Arc::new(CounterSaga::new(format!("saga:restart:{}", tag), saga_id));
    source_bus2
        .subscribe(SagaAdapter::new(
            saga2.clone(),
            foreign_sub_id.clone(),
            |e: &SourceEvent| match e {
                SourceEvent::SourceTick { saga_id, value } => Some(TargetEvent::BridgedTick {
                    saga_id: *saga_id,
                    value: *value,
                }),
            },
        ))
        .await
        .unwrap();
    source_bus2.start_listener().await.unwrap();

    source_store2
        .store_event(source_event(stream_id, 2, saga_id, 2))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let handled = saga2.handled.lock().await;
    assert_eq!(
        handled.len(),
        1,
        "after restart, saga should only see the new event (not replay)"
    );
    assert!(
        matches!(handled[0].data, TargetEvent::BridgedTick { value: 2, .. }),
        "the one event should be the new one (value=2), got {:?}",
        handled[0].data
    );

    let cp_after_second = fetch_checkpoint(&pool, &foreign_sub_id).await.unwrap();
    assert!(
        cp_after_second > cp_after_first,
        "checkpoint should advance ({} -> {})",
        cp_after_first,
        cp_after_second
    );
}
