//! Migration integration tests.
//!
//! These tests are **destructive**: every test drops all epoch tables (see
//! [`teardown`]) to exercise the migrator from a clean slate. They therefore
//! run against a dedicated database (`epoch_pg_test_migrations`) instead of
//! the shared test database — `cargo test --workspace` runs test binaries as
//! parallel processes, and `#[serial]` only serializes within one binary, so
//! dropping tables in the shared database would race the other integration
//! suites.

mod common;

use epoch_pg::migrations::{MIGRATION_COUNT, Migrator};
use serial_test::serial;
use sqlx::PgPool;

async fn teardown(pool: &PgPool) {
    // Drop all epoch tables in correct order (respecting foreign keys)
    sqlx::query("DROP INDEX IF EXISTS idx_epoch_checkpoints_global_sequence CASCADE")
        .execute(pool)
        .await
        .expect("Failed to drop checkpoint index");
    sqlx::query("DROP TABLE IF EXISTS epoch_event_bus_gap_timeouts CASCADE")
        .execute(pool)
        .await
        .expect("Failed to drop gap_timeouts table");
    sqlx::query("DROP TABLE IF EXISTS epoch_event_bus_dlq CASCADE")
        .execute(pool)
        .await
        .expect("Failed to drop dlq table");
    sqlx::query("DROP TABLE IF EXISTS epoch_event_bus_checkpoints CASCADE")
        .execute(pool)
        .await
        .expect("Failed to drop checkpoints table");
    sqlx::query("DROP TABLE IF EXISTS epoch_events CASCADE")
        .execute(pool)
        .await
        .expect("Failed to drop events table");
    sqlx::query("DROP TABLE IF EXISTS epoch_events_legacy CASCADE")
        .execute(pool)
        .await
        .expect("Failed to drop legacy events table");
    sqlx::query("DROP TABLE IF EXISTS _epoch_migrations CASCADE")
        .execute(pool)
        .await
        .expect("Failed to drop migrations table");
    sqlx::query("DROP FUNCTION IF EXISTS epoch_notify_event CASCADE")
        .execute(pool)
        .await
        .expect("Failed to drop notify function");
    sqlx::query("DROP SEQUENCE IF EXISTS epoch_events_global_sequence_seq CASCADE")
        .execute(pool)
        .await
        .expect("Failed to drop sequence");
}

#[tokio::test]
#[serial]
async fn test_migrator_creates_tracking_table() {
    let Some(pool) = common::try_get_pg_pool_for_db("epoch_pg_test_migrations").await else {
        return;
    };
    teardown(&pool).await;

    let migrator = Migrator::new(pool.clone());

    // Just calling current_version should create the tracking table
    let version = migrator
        .current_version()
        .await
        .expect("Should get version");
    assert_eq!(version, 0, "Initial version should be 0");

    // Verify the tracking table exists
    let result: (i64,) = sqlx::query_as(
        r#"
        SELECT COUNT(*) 
        FROM information_schema.tables 
        WHERE table_name = '_epoch_migrations'
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("Failed to query information_schema");

    assert_eq!(result.0, 1, "_epoch_migrations table should exist");

    teardown(&pool).await;
}

#[tokio::test]
#[serial]
async fn test_migrator_runs_all_migrations() {
    let Some(pool) = common::try_get_pg_pool_for_db("epoch_pg_test_migrations").await else {
        return;
    };
    teardown(&pool).await;

    let migrator = Migrator::new(pool.clone());

    // Run migrations
    let applied = migrator.run().await.expect("Should run migrations");
    assert_eq!(
        applied, MIGRATION_COUNT,
        "Should apply all registered migrations"
    );

    // Verify current version
    let version = migrator
        .current_version()
        .await
        .expect("Should get version");
    assert_eq!(
        version, MIGRATION_COUNT as i64,
        "Version should be the latest migration after all migrations"
    );

    // Verify epoch_events table exists
    let events_table: (i64,) = sqlx::query_as(
        r#"
        SELECT COUNT(*) 
        FROM information_schema.tables 
        WHERE table_name = 'epoch_events'
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("Failed to query");
    assert_eq!(events_table.0, 1, "epoch_events table should exist");

    // Verify global_sequence column exists
    let global_seq_col: (i64,) = sqlx::query_as(
        r#"
        SELECT COUNT(*) 
        FROM information_schema.columns 
        WHERE table_name = 'epoch_events' AND column_name = 'global_sequence'
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("Failed to query");
    assert_eq!(global_seq_col.0, 1, "global_sequence column should exist");

    // Verify checkpoint table exists
    let checkpoints_table: (i64,) = sqlx::query_as(
        r#"
        SELECT COUNT(*) 
        FROM information_schema.tables 
        WHERE table_name = 'epoch_event_bus_checkpoints'
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("Failed to query");
    assert_eq!(
        checkpoints_table.0, 1,
        "epoch_event_bus_checkpoints table should exist"
    );

    // Verify DLQ table exists
    let dlq_table: (i64,) = sqlx::query_as(
        r#"
        SELECT COUNT(*) 
        FROM information_schema.tables 
        WHERE table_name = 'epoch_event_bus_dlq'
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("Failed to query");
    assert_eq!(dlq_table.0, 1, "epoch_event_bus_dlq table should exist");

    // Verify gap-timeout table exists
    let gap_timeout_table: (i64,) = sqlx::query_as(
        r#"
        SELECT COUNT(*) 
        FROM information_schema.tables 
        WHERE table_name = 'epoch_event_bus_gap_timeouts'
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("Failed to query");
    assert_eq!(
        gap_timeout_table.0, 1,
        "epoch_event_bus_gap_timeouts table should exist"
    );

    teardown(&pool).await;
}

#[tokio::test]
#[serial]
async fn test_migrator_is_idempotent() {
    let Some(pool) = common::try_get_pg_pool_for_db("epoch_pg_test_migrations").await else {
        return;
    };
    teardown(&pool).await;

    let migrator = Migrator::new(pool.clone());

    // Run migrations first time
    let applied1 = migrator.run().await.expect("Should run migrations");
    assert_eq!(
        applied1, MIGRATION_COUNT,
        "Should apply all registered migrations first time"
    );

    // Run migrations second time
    let applied2 = migrator.run().await.expect("Should run migrations again");
    assert_eq!(applied2, 0, "Should apply 0 migrations second time");

    // Run migrations third time
    let applied3 = migrator
        .run()
        .await
        .expect("Should run migrations third time");
    assert_eq!(applied3, 0, "Should apply 0 migrations third time");

    // Version should still be the latest migration
    let version = migrator
        .current_version()
        .await
        .expect("Should get version");
    assert_eq!(
        version, MIGRATION_COUNT as i64,
        "Version should still be the latest migration"
    );

    teardown(&pool).await;
}

#[tokio::test]
#[serial]
async fn test_migrator_pending_returns_unapplied_migrations() {
    let Some(pool) = common::try_get_pg_pool_for_db("epoch_pg_test_migrations").await else {
        return;
    };
    teardown(&pool).await;

    let migrator = Migrator::new(pool.clone());

    // Before running, all should be pending
    let pending = migrator.pending().await.expect("Should get pending");
    assert_eq!(
        pending.len(),
        MIGRATION_COUNT,
        "All registered migrations should be pending"
    );

    // Run migrations
    migrator.run().await.expect("Should run migrations");

    // After running, none should be pending
    let pending_after = migrator.pending().await.expect("Should get pending");
    assert_eq!(pending_after.len(), 0, "No migrations should be pending");

    teardown(&pool).await;
}

#[tokio::test]
#[serial]
async fn test_migrator_applied_returns_applied_migrations() {
    let Some(pool) = common::try_get_pg_pool_for_db("epoch_pg_test_migrations").await else {
        return;
    };
    teardown(&pool).await;

    let migrator = Migrator::new(pool.clone());

    // Before running, none should be applied
    let applied = migrator.applied().await.expect("Should get applied");
    assert_eq!(
        applied.len(),
        0,
        "No migrations should be applied initially"
    );

    // Run migrations
    migrator.run().await.expect("Should run migrations");

    // After running, all should be applied
    let applied_after = migrator.applied().await.expect("Should get applied");
    assert_eq!(
        applied_after.len(),
        MIGRATION_COUNT,
        "All registered migrations should be applied"
    );

    // Verify the applied migrations have correct data
    assert_eq!(applied_after[0].version, 1);
    assert_eq!(applied_after[0].name, "create_events_table");
    assert!(!applied_after[0].checksum.is_empty());

    assert_eq!(applied_after[1].version, 2);
    assert_eq!(applied_after[1].name, "add_global_sequence");

    assert_eq!(applied_after[2].version, 3);
    assert_eq!(applied_after[2].name, "create_event_bus_infrastructure");

    assert_eq!(applied_after[3].version, 4);
    assert_eq!(applied_after[3].name, "rename_tables_with_epoch_prefix");

    assert_eq!(applied_after[4].version, 5);
    assert_eq!(applied_after[4].name, "add_checkpoint_sequence_index");

    assert_eq!(applied_after[5].version, 6);
    assert_eq!(applied_after[5].name, "add_dlq_resolution_columns");

    assert_eq!(applied_after[6].version, 7);
    assert_eq!(applied_after[6].name, "add_causation_columns");

    assert_eq!(applied_after[7].version, 8);
    assert_eq!(applied_after[7].name, "add_bus_name_to_checkpoints");

    assert_eq!(applied_after[8].version, 9);
    assert_eq!(applied_after[8].name, "create_gap_timeout_log");

    assert_eq!(applied_after[9].version, 10);
    assert_eq!(applied_after[9].name, "strip_data_from_notify_payload");

    assert_eq!(applied_after[10].version, 11);
    assert_eq!(applied_after[10].name, "add_txid_to_events");

    teardown(&pool).await;
}

#[tokio::test]
#[serial]
async fn test_m011_skips_when_epoch_events_renamed() {
    // Reproduces the post-R28-cutover failure: `epoch_events` renamed to
    // `epoch_events_legacy` before m011 is applied. The migration must succeed
    // as a no-op instead of aborting with `relation "epoch_events" does not exist`.
    let Some(pool) = common::try_get_pg_pool_for_db("epoch_pg_test_migrations").await else {
        return;
    };
    teardown(&pool).await;

    let migrator = Migrator::new(pool.clone());

    // Apply all migrations on a clean DB (m011 runs normally, adds txid).
    migrator
        .run()
        .await
        .expect("Initial migration run should succeed");

    // Simulate the R28 cutover **after** m011 has run, then roll back m011's
    // tracking record so the migrator will attempt to re-apply it.
    sqlx::query("DROP INDEX IF EXISTS idx_epoch_events_txid")
        .execute(&pool)
        .await
        .expect("Drop txid index");
    sqlx::query("ALTER TABLE epoch_events DROP COLUMN IF EXISTS txid")
        .execute(&pool)
        .await
        .expect("Drop txid column");
    sqlx::query("ALTER TABLE epoch_events RENAME TO epoch_events_legacy")
        .execute(&pool)
        .await
        .expect("Rename epoch_events → epoch_events_legacy");
    sqlx::query("DELETE FROM _epoch_migrations WHERE version = 11")
        .execute(&pool)
        .await
        .expect("Remove m011 tracking record");

    // This is the assertion that fails with the unfixed migration:
    // `relation "epoch_events" does not exist` → MigrationFailed.
    migrator
        .run()
        .await
        .expect("m011 must succeed as a no-op when epoch_events does not exist");

    // m011 should now be re-recorded as applied.
    let applied_after = migrator.applied().await.expect("Should get applied");
    let m011_applied = applied_after.iter().any(|m| m.version == 11);
    assert!(
        m011_applied,
        "m011 should be recorded as applied after the no-op run"
    );

    // No `epoch_events` should have been (re-)created.
    let epoch_events_exists: bool =
        sqlx::query_scalar("SELECT to_regclass('epoch_events') IS NOT NULL")
            .fetch_one(&pool)
            .await
            .expect("to_regclass query");
    assert!(
        !epoch_events_exists,
        "epoch_events must not be recreated by the no-op migration"
    );

    // The renamed table must still be intact.
    let legacy_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = 'epoch_events_legacy')",
    )
    .fetch_one(&pool)
    .await
    .expect("legacy table check");
    assert!(
        legacy_exists,
        "epoch_events_legacy should still exist after the no-op run"
    );

    // Cleanup: drop the legacy table so teardown() can proceed cleanly.
    sqlx::query("DROP TABLE IF EXISTS epoch_events_legacy CASCADE")
        .execute(&pool)
        .await
        .expect("Drop epoch_events_legacy");
    teardown(&pool).await;
}

#[tokio::test]
#[serial]
async fn test_m011_adds_txid_column_when_table_present() {
    // Regression test: on a normal (non-cutover) database, m011 must still add
    // the txid column, its DEFAULT, and the partial index.
    let Some(pool) = common::try_get_pg_pool_for_db("epoch_pg_test_migrations").await else {
        return;
    };
    teardown(&pool).await;

    let migrator = Migrator::new(pool.clone());
    migrator.run().await.expect("Migration run should succeed");

    // Assert the txid column exists on epoch_events.
    let col_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (\
            SELECT 1 FROM information_schema.columns \
            WHERE table_name = 'epoch_events' AND column_name = 'txid'\
        )",
    )
    .fetch_one(&pool)
    .await
    .expect("Column existence check");
    assert!(
        col_exists,
        "txid column should exist on epoch_events after m011"
    );

    // Assert the partial index exists.
    let idx_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_indexes WHERE indexname = 'idx_epoch_events_txid')",
    )
    .fetch_one(&pool)
    .await
    .expect("Index existence check");
    assert!(idx_exists, "idx_epoch_events_txid should exist after m011");

    teardown(&pool).await;
}

#[tokio::test]
#[serial]
async fn test_migrator_records_checksums() {
    let Some(pool) = common::try_get_pg_pool_for_db("epoch_pg_test_migrations").await else {
        return;
    };
    teardown(&pool).await;

    let migrator = Migrator::new(pool.clone());
    migrator.run().await.expect("Should run migrations");

    // Verify checksums are recorded
    let applied = migrator.applied().await.expect("Should get applied");

    for migration in &applied {
        assert!(
            !migration.checksum.is_empty(),
            "Migration {} should have a checksum",
            migration.name
        );
        // SHA-256 hex is 64 characters
        assert_eq!(
            migration.checksum.len(),
            64,
            "Checksum should be 64 hex characters"
        );
    }

    teardown(&pool).await;
}
