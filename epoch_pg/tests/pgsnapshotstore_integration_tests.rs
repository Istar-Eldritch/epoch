//! Integration tests for [`PgSnapshotStore`].
//!
//! Each test uses a fresh [`Uuid`] as the `stream_id` to remain fully
//! independent of other tests even when run in parallel against a shared DB.

mod common;

use epoch_core::{
    event::{EnumConversionError, Event, EventData},
    event_applicator::EventApplicatorState,
    prelude::{EventApplicator, EventStoreBackend},
    snapshot::{SnapshotRetention, SnapshotStore, state_at},
};
use epoch_mem::{InMemoryEventBus, InMemoryStateStore};
use epoch_pg::{PgSnapshotStore, event_store::PgEventStore, migrations::Migrator};
use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};
use serial_test::serial;
use uuid::Uuid;

/// Simple serialisable state type used across all snapshot tests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct TestState {
    value: String,
}

impl TestState {
    fn new(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
        }
    }
}

/// Ensures migrations are applied so `epoch_snapshots` exists.
async fn setup(pool: &sqlx::PgPool) {
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("migrations must succeed");
}

// ---------------------------------------------------------------------------
// load_snapshot — nearest ≤
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn test_save_and_load_nearest() {
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    setup(&pool).await;

    let store: PgSnapshotStore<TestState> = PgSnapshotStore::new(pool.clone());
    let stream_id = Uuid::new_v4();

    store
        .save_snapshot(stream_id, 2, &TestState::new("v2"))
        .await
        .expect("save v2");
    store
        .save_snapshot(stream_id, 5, &TestState::new("v5"))
        .await
        .expect("save v5");
    store
        .save_snapshot(stream_id, 9, &TestState::new("v9"))
        .await
        .expect("save v9");

    // load(7) → v5 (nearest ≤ 7)
    let snap = store
        .load_snapshot(stream_id, 7)
        .await
        .expect("load")
        .expect("should find v5");
    assert_eq!(snap.version, 5);
    assert_eq!(snap.state, TestState::new("v5"));

    // load(9) → v9 (exact match)
    let snap = store
        .load_snapshot(stream_id, 9)
        .await
        .expect("load")
        .expect("should find v9");
    assert_eq!(snap.version, 9);
    assert_eq!(snap.state, TestState::new("v9"));

    // load(100) → v9 (highest available)
    let snap = store
        .load_snapshot(stream_id, 100)
        .await
        .expect("load")
        .expect("should find v9");
    assert_eq!(snap.version, 9);

    // load(1) → None (nothing at or before 1)
    let snap = store.load_snapshot(stream_id, 1).await.expect("load");
    assert!(snap.is_none(), "expected None for version 1");
}

// ---------------------------------------------------------------------------
// save_snapshot — idempotency
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn test_save_snapshot_idempotent() {
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    setup(&pool).await;

    let store: PgSnapshotStore<TestState> = PgSnapshotStore::new(pool.clone());
    let stream_id = Uuid::new_v4();

    // Save v5 twice with different state; second must win.
    store
        .save_snapshot(stream_id, 5, &TestState::new("first"))
        .await
        .expect("save first");
    store
        .save_snapshot(stream_id, 5, &TestState::new("second"))
        .await
        .expect("save second");

    // Only one row at v5; latest state wins.
    let snap = store
        .load_snapshot(stream_id, 5)
        .await
        .expect("load")
        .expect("should find v5");
    assert_eq!(snap.version, 5);
    assert_eq!(snap.state, TestState::new("second"));

    // Confirm there is exactly one row for this stream.
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM epoch_snapshots WHERE stream_id = $1")
        .bind(stream_id)
        .fetch_one(&pool)
        .await
        .expect("count query");
    assert_eq!(count.0, 1, "must be exactly one row at v5");
}

// ---------------------------------------------------------------------------
// apply_retention — KeepLast + Unlimited
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn test_keep_last_prunes() {
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    setup(&pool).await;

    let store: PgSnapshotStore<TestState> = PgSnapshotStore::new(pool.clone());
    let stream_id = Uuid::new_v4();

    for v in [2u64, 5, 9] {
        store
            .save_snapshot(stream_id, v, &TestState::new(format!("v{v}")))
            .await
            .expect("save");
    }

    // KeepLast(2) should leave {v5, v9}.
    store
        .apply_retention(stream_id, &SnapshotRetention::KeepLast(2))
        .await
        .expect("apply_retention");

    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM epoch_snapshots WHERE stream_id = $1")
        .bind(stream_id)
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(count.0, 2, "KeepLast(2) must leave exactly 2 rows");

    // v2 must be gone; v9 must remain.
    let v2 = store.load_snapshot(stream_id, 2).await.expect("load");
    assert!(v2.is_none(), "v2 should have been pruned");

    let v9 = store
        .load_snapshot(stream_id, 9)
        .await
        .expect("load")
        .expect("v9 must survive");
    assert_eq!(v9.version, 9);
}

#[tokio::test]
#[serial]
async fn test_unlimited_retention_no_op() {
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    setup(&pool).await;

    let store: PgSnapshotStore<TestState> = PgSnapshotStore::new(pool.clone());
    let stream_id = Uuid::new_v4();

    for v in [2u64, 5, 9] {
        store
            .save_snapshot(stream_id, v, &TestState::new(format!("v{v}")))
            .await
            .expect("save");
    }

    store
        .apply_retention(stream_id, &SnapshotRetention::Unlimited)
        .await
        .expect("apply_retention");

    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM epoch_snapshots WHERE stream_id = $1")
        .bind(stream_id)
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(count.0, 3, "Unlimited must leave all rows intact");
}

#[tokio::test]
#[serial]
async fn test_keep_last_zero_empties() {
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    setup(&pool).await;

    let store: PgSnapshotStore<TestState> = PgSnapshotStore::new(pool.clone());
    let stream_id = Uuid::new_v4();

    for v in [2u64, 5, 9] {
        store
            .save_snapshot(stream_id, v, &TestState::new(format!("v{v}")))
            .await
            .expect("save");
    }

    store
        .apply_retention(stream_id, &SnapshotRetention::KeepLast(0))
        .await
        .expect("apply_retention");

    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM epoch_snapshots WHERE stream_id = $1")
        .bind(stream_id)
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(count.0, 0, "KeepLast(0) must remove all rows");
}

// ---------------------------------------------------------------------------
// migration: epoch_snapshots table exists after m013
// ---------------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn test_epoch_snapshots_table_exists_after_migration() {
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    setup(&pool).await;

    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = 'epoch_snapshots')",
    )
    .fetch_one(&pool)
    .await
    .expect("table existence check");

    assert!(exists, "epoch_snapshots table must exist after m013");

    // Verify the index exists.
    let idx_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_indexes WHERE indexname = 'idx_epoch_snapshots_stream_version')",
    )
    .fetch_one(&pool)
    .await
    .expect("index existence check");

    assert!(
        idx_exists,
        "idx_epoch_snapshots_stream_version must exist after m013"
    );
}

// ---------------------------------------------------------------------------
// state_at — equivalence with full replay
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CounterEventData {
    delta: i64,
}

impl EventData for CounterEventData {
    fn event_type(&self) -> &'static str {
        "CounterEvent"
    }
}

impl TryFrom<&CounterEventData> for CounterEventData {
    type Error = EnumConversionError;
    fn try_from(v: &CounterEventData) -> Result<Self, Self::Error> {
        Ok(v.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CounterState {
    id: Uuid,
    version: u64,
    total: i64,
}

impl EventApplicatorState for CounterState {
    fn get_id(&self) -> &Uuid {
        &self.id
    }
}

#[derive(Debug, thiserror::Error)]
#[error("counter apply error")]
struct CounterApplyError;

struct CounterApplicator {
    id: Uuid,
    state_store: InMemoryStateStore<CounterState>,
}

impl CounterApplicator {
    fn new(id: Uuid) -> Self {
        Self {
            id,
            state_store: InMemoryStateStore::new(),
        }
    }
}

impl EventApplicator<CounterEventData> for CounterApplicator {
    type State = CounterState;
    type StateStore = InMemoryStateStore<CounterState>;
    type EventType = CounterEventData;
    type ApplyError = CounterApplyError;

    fn get_state_store(&self) -> Self::StateStore {
        self.state_store.clone()
    }

    fn apply(
        &self,
        state: Option<Self::State>,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::ApplyError> {
        let delta = event.data.as_ref().map(|d| d.delta).unwrap_or(0);
        let next = match state {
            Some(mut s) => {
                s.version = event.stream_version;
                s.total += delta;
                s
            }
            None => CounterState {
                id: self.id,
                version: event.stream_version,
                total: delta,
            },
        };
        Ok(Some(next))
    }
}

fn counter_event(stream_id: Uuid, version: u64, delta: i64) -> Event<CounterEventData> {
    Event::<CounterEventData>::builder()
        .stream_id(stream_id)
        .event_type("CounterEvent".to_string())
        .stream_version(version)
        .data(Some(CounterEventData { delta }))
        .build()
        .unwrap()
}

type PgStoreErr =
    epoch_pg::event_store::PgEventStoreError<epoch_mem::InMemoryEventBusError<CounterEventData>>;

/// Full replay baseline: read all events from version 1 and truncate at `target`.
async fn full_replay_pg(
    applicator: &CounterApplicator,
    event_store: &PgEventStore<InMemoryEventBus<CounterEventData>>,
    stream_id: Uuid,
    target: u64,
) -> Option<CounterState> {
    use epoch_core::event_store::SliceEventStream;
    use std::pin::Pin;

    let mut raw = event_store.read_events_since(stream_id, 1).await.unwrap();
    let mut collected: Vec<Event<CounterEventData>> = Vec::new();
    while let Some(result) = raw.next().await {
        let ev = result.unwrap();
        if ev.stream_version > target {
            break;
        }
        collected.push(ev);
    }
    let stream: Pin<
        Box<dyn epoch_core::event_store::EventStream<CounterEventData, PgStoreErr> + Send + '_>,
    > = Box::pin(SliceEventStream::from(collected.as_slice()));
    applicator.re_hydrate(None, stream).await.unwrap()
}

/// Verifies that `state_at(v)` equals a full replay from zero for every `v` in 1..=N,
/// exercising both the snapshot fast-path and the no-snapshot path.
#[tokio::test]
#[serial]
async fn test_state_at_matches_full_replay() {
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    setup(&pool).await;

    let stream_id = Uuid::new_v4();
    let bus = InMemoryEventBus::<CounterEventData>::new();
    let event_store = PgEventStore::new(pool.clone(), bus);
    let snapshot_store: PgSnapshotStore<CounterState> = PgSnapshotStore::new(pool.clone());
    let applicator = CounterApplicator::new(stream_id);

    const N: u64 = 10;
    for v in 1..=N {
        event_store
            .store_event(counter_event(stream_id, v, 1))
            .await
            .unwrap();
    }

    // Place snapshots at versions 3 and 7.
    let snap3 = full_replay_pg(&applicator, &event_store, stream_id, 3)
        .await
        .unwrap();
    let snap7 = full_replay_pg(&applicator, &event_store, stream_id, 7)
        .await
        .unwrap();
    snapshot_store
        .save_snapshot(stream_id, 3, &snap3)
        .await
        .unwrap();
    snapshot_store
        .save_snapshot(stream_id, 7, &snap7)
        .await
        .unwrap();

    for v in 1..=N {
        let via_state_at = state_at(&applicator, &event_store, &snapshot_store, stream_id, v)
            .await
            .unwrap();
        let baseline = full_replay_pg(&applicator, &event_store, stream_id, v).await;
        assert_eq!(
            via_state_at, baseline,
            "state_at({v}) != full_replay({v}) on PG"
        );
    }
}
