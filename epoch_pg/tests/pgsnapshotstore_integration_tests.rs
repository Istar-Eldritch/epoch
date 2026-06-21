//! Integration tests for [`PgSnapshotStore`].
//!
//! Each test uses a fresh [`Uuid`] as the `stream_id` to remain fully
//! independent of other tests even when run in parallel against a shared DB.

mod common;

use epoch_core::snapshot::{SnapshotRetention, SnapshotStore};
use epoch_pg::{PgSnapshotStore, migrations::Migrator};
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
