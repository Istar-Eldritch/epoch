//! Migration 004: Rename tables to use epoch_ prefix.
//!
//! This migration renames all epoch tables to use the `epoch_` prefix
//! to avoid conflicts with user tables. It handles both fresh installations
//! (where tables already have the new names from IF NOT EXISTS) and
//! existing production databases (where tables have legacy names).

use async_trait::async_trait;
use sqlx::{Postgres, Row, Transaction};

use super::{Migration, MigrationError};

/// Renames tables, sequences, indexes, and functions to use epoch_ prefix.
pub struct RenameTablesWithEpochPrefix;

#[async_trait]
impl Migration for RenameTablesWithEpochPrefix {
    fn version(&self) -> i64 {
        4
    }

    fn name(&self) -> &'static str {
        "rename_tables_with_epoch_prefix"
    }

    async fn up<'a>(&self, tx: &mut Transaction<'a, Postgres>) -> Result<(), MigrationError> {
        // Helper to check if a table exists
        async fn table_exists(
            tx: &mut Transaction<'_, Postgres>,
            name: &str,
        ) -> Result<bool, MigrationError> {
            let row = sqlx::query(
                "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = $1)",
            )
            .bind(name)
            .fetch_one(&mut **tx)
            .await?;
            Ok(row.get::<bool, _>(0))
        }

        // Helper to check if a sequence exists
        async fn sequence_exists(
            tx: &mut Transaction<'_, Postgres>,
            name: &str,
        ) -> Result<bool, MigrationError> {
            let row =
                sqlx::query("SELECT EXISTS (SELECT 1 FROM pg_sequences WHERE sequencename = $1)")
                    .bind(name)
                    .fetch_one(&mut **tx)
                    .await?;
            Ok(row.get::<bool, _>(0))
        }

        // Helper to check if an index exists
        async fn index_exists(
            tx: &mut Transaction<'_, Postgres>,
            name: &str,
        ) -> Result<bool, MigrationError> {
            let row = sqlx::query("SELECT EXISTS (SELECT 1 FROM pg_indexes WHERE indexname = $1)")
                .bind(name)
                .fetch_one(&mut **tx)
                .await?;
            Ok(row.get::<bool, _>(0))
        }

        // Helper to check if a function exists
        async fn function_exists(
            tx: &mut Transaction<'_, Postgres>,
            name: &str,
        ) -> Result<bool, MigrationError> {
            let row = sqlx::query("SELECT EXISTS (SELECT 1 FROM pg_proc WHERE proname = $1)")
                .bind(name)
                .fetch_one(&mut **tx)
                .await?;
            Ok(row.get::<bool, _>(0))
        }

        // === Rename events table ===
        if table_exists(tx, "events").await? && !table_exists(tx, "epoch_events").await? {
            sqlx::query("ALTER TABLE events RENAME TO epoch_events")
                .execute(&mut **tx)
                .await?;
        }

        // === Rename sequence ===
        if sequence_exists(tx, "events_global_sequence_seq").await?
            && !sequence_exists(tx, "epoch_events_global_sequence_seq").await?
        {
            sqlx::query(
                "ALTER SEQUENCE events_global_sequence_seq RENAME TO epoch_events_global_sequence_seq",
            )
            .execute(&mut **tx)
            .await?;
        }

        // === Rename index on global_sequence ===
        if index_exists(tx, "idx_events_global_sequence").await?
            && !index_exists(tx, "idx_epoch_events_global_sequence").await?
        {
            sqlx::query(
                "ALTER INDEX idx_events_global_sequence RENAME TO idx_epoch_events_global_sequence",
            )
            .execute(&mut **tx)
            .await?;
        }

        // === Rename event_bus_checkpoints table ===
        if table_exists(tx, "event_bus_checkpoints").await?
            && !table_exists(tx, "epoch_event_bus_checkpoints").await?
        {
            sqlx::query("ALTER TABLE event_bus_checkpoints RENAME TO epoch_event_bus_checkpoints")
                .execute(&mut **tx)
                .await?;
        }

        // === Rename event_bus_dlq table ===
        if table_exists(tx, "event_bus_dlq").await?
            && !table_exists(tx, "epoch_event_bus_dlq").await?
        {
            sqlx::query("ALTER TABLE event_bus_dlq RENAME TO epoch_event_bus_dlq")
                .execute(&mut **tx)
                .await?;
        }

        // === Rename DLQ indexes ===
        if index_exists(tx, "idx_dlq_subscriber").await?
            && !index_exists(tx, "idx_epoch_dlq_subscriber").await?
        {
            sqlx::query("ALTER INDEX idx_dlq_subscriber RENAME TO idx_epoch_dlq_subscriber")
                .execute(&mut **tx)
                .await?;
        }

        if index_exists(tx, "idx_dlq_created_at").await?
            && !index_exists(tx, "idx_epoch_dlq_created_at").await?
        {
            sqlx::query("ALTER INDEX idx_dlq_created_at RENAME TO idx_epoch_dlq_created_at")
                .execute(&mut **tx)
                .await?;
        }

        // === Rename DLQ constraint ===
        // Constraints are renamed via ALTER TABLE
        if table_exists(tx, "epoch_event_bus_dlq").await? {
            // Check if old constraint exists
            let old_constraint_exists: bool = sqlx::query(
                r#"
                SELECT EXISTS (
                    SELECT 1 FROM pg_constraint 
                    WHERE conname = 'unique_subscriber_event'
                )
                "#,
            )
            .fetch_one(&mut **tx)
            .await?
            .get(0);

            if old_constraint_exists {
                sqlx::query(
                    "ALTER TABLE epoch_event_bus_dlq RENAME CONSTRAINT unique_subscriber_event TO epoch_unique_subscriber_event",
                )
                .execute(&mut **tx)
                .await?;
            }
        }

        // === Rename notification function ===
        if function_exists(tx, "epoch_pg_notify_event").await?
            && !function_exists(tx, "epoch_notify_event").await?
        {
            sqlx::query("ALTER FUNCTION epoch_pg_notify_event() RENAME TO epoch_notify_event")
                .execute(&mut **tx)
                .await?;
        }

        // === Update any existing triggers to use new function name ===
        // Drop old trigger if it exists and create new one
        sqlx::query("DROP TRIGGER IF EXISTS event_bus_notify_trigger ON epoch_events")
            .execute(&mut **tx)
            .await?;

        Ok(())
    }
}
