mod common;

use epoch_pg::migrations::Migrator;
use serial_test::serial;
use sqlx::PgPool;

async fn teardown(pool: &PgPool) {
    // Drop all epoch tables in correct order (respecting foreign keys)
    sqlx::query("DROP INDEX IF EXISTS idx_epoch_checkpoints_global_sequence CASCADE")
        .execute(pool)
        .await
        .expect("Failed to drop checkpoint index");
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
    let pool = common::get_pg_pool().await;
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
    let pool = common::get_pg_pool().await;
    teardown(&pool).await;

    let migrator = Migrator::new(pool.clone());

    // Run migrations
    let applied = migrator.run().await.expect("Should run migrations");
    assert_eq!(applied, 7, "Should apply 7 migrations");

    // Verify current version
    let version = migrator
        .current_version()
        .await
        .expect("Should get version");
    assert_eq!(version, 7, "Version should be 7 after all migrations");

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

    teardown(&pool).await;
}

#[tokio::test]
#[serial]
async fn test_migrator_is_idempotent() {
    let pool = common::get_pg_pool().await;
    teardown(&pool).await;

    let migrator = Migrator::new(pool.clone());

    // Run migrations first time
    let applied1 = migrator.run().await.expect("Should run migrations");
    assert_eq!(applied1, 7, "Should apply 7 migrations first time");

    // Run migrations second time
    let applied2 = migrator.run().await.expect("Should run migrations again");
    assert_eq!(applied2, 0, "Should apply 0 migrations second time");

    // Run migrations third time
    let applied3 = migrator
        .run()
        .await
        .expect("Should run migrations third time");
    assert_eq!(applied3, 0, "Should apply 0 migrations third time");

    // Version should still be 7
    let version = migrator
        .current_version()
        .await
        .expect("Should get version");
    assert_eq!(version, 7, "Version should still be 7");

    teardown(&pool).await;
}

#[tokio::test]
#[serial]
async fn test_migrator_pending_returns_unapplied_migrations() {
    let pool = common::get_pg_pool().await;
    teardown(&pool).await;

    let migrator = Migrator::new(pool.clone());

    // Before running, all should be pending
    let pending = migrator.pending().await.expect("Should get pending");
    assert_eq!(pending.len(), 7, "All 7 migrations should be pending");

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
    let pool = common::get_pg_pool().await;
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
    assert_eq!(applied_after.len(), 7, "All 7 migrations should be applied");

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

    teardown(&pool).await;
}

#[tokio::test]
#[serial]
async fn test_migrator_records_checksums() {
    let pool = common::get_pg_pool().await;
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
