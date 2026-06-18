//! Migration 012: Add `schema_version INT` column to `epoch_events`.
//!
//! This migration adds a nullable `schema_version INT` column with a
//! `DEFAULT 1` to the `epoch_events` table. The column is used by the
//! upcasting mechanism (CLOUD-173) to track which version of the event
//! schema a stored payload was written with.
//!
//! **No backfill** is performed: rows written before this migration keep
//! `schema_version = NULL`. The read path interprets `NULL` as version `1`
//! (the universal floor) via `entry.schema_version.unwrap_or(1)`, so legacy
//! events replay correctly without any backfill step.
//!
//! New inserts that do not explicitly supply a `schema_version` are stamped
//! `1` by the column `DEFAULT`, ensuring they are indistinguishable from
//! legacy rows to the upcasting chain.
//!
//! **Table-rename guard:** if `epoch_events` does not exist (e.g. after the
//! R28 domain-table cutover that renames it to `epoch_events_legacy`), the
//! migration is a deliberate **no-op** (with a `WARN` log), following the
//! same pattern established by m011. Any post-cutover active table receives
//! the column via `event_bus::ensure_schema_version_column()`.

use async_trait::async_trait;
use sqlx::{Postgres, Transaction};

use super::{Migration, MigrationError};

/// Migration 012: add nullable `schema_version INT DEFAULT 1` to `epoch_events`.
pub struct AddSchemaVersionToEvents;

#[async_trait]
impl Migration for AddSchemaVersionToEvents {
    fn version(&self) -> i64 {
        12
    }

    fn name(&self) -> &'static str {
        "add_schema_version_to_events"
    }

    async fn up<'a>(&self, tx: &mut Transaction<'a, Postgres>) -> Result<(), MigrationError> {
        // Table-rename guard (mirrors m011): tolerate the epoch_events rename.
        //
        // After the R28 domain-table cutover, `epoch_events` may have been
        // renamed. `ADD COLUMN IF NOT EXISTS` guards the *column*, not the
        // *table*, so without this guard the migration would fail with
        // `relation "epoch_events" does not exist` and abort boot.
        //
        // When the table is absent the migration is a deliberate no-op: the
        // legacy table is frozen and not migrated, and any post-cutover active
        // table receives `schema_version` via
        // `event_bus::ensure_schema_version_column()`.
        let table_exists: bool =
            sqlx::query_scalar("SELECT to_regclass('epoch_events') IS NOT NULL")
                .fetch_one(&mut **tx)
                .await?;

        if !table_exists {
            log::warn!(
                "m012 (add_schema_version_to_events): table 'epoch_events' not found \
                 (expected after the R28 table-rename cutover); skipping schema_version \
                 column and default as a no-op."
            );
            return Ok(());
        }

        // Step 1: add the nullable column (metadata-only; no table rewrite).
        // Existing rows receive schema_version = NULL, which the read path
        // treats as version 1 via `unwrap_or(1)`.
        sqlx::query(
            r#"
            ALTER TABLE epoch_events
                ADD COLUMN IF NOT EXISTS schema_version INT
            "#,
        )
        .execute(&mut **tx)
        .await?;

        // Step 2: set the column DEFAULT so new inserts without an explicit
        // schema_version are stamped 1.
        sqlx::query(
            r#"
            ALTER TABLE epoch_events
                ALTER COLUMN schema_version SET DEFAULT 1
            "#,
        )
        .execute(&mut **tx)
        .await?;

        Ok(())
    }
}
