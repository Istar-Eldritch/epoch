//! Migration 008: Add `bus_name` column and widen the PK to `(bus_name, subscriber_id)`.
//!
//! Required for per-domain buses — each bus uses its own `events_table` as the
//! `bus_name` key so checkpoints remain isolated in the shared table.
//! Existing rows are backfilled to `'epoch_events'` (the legacy default), then the
//! default is dropped so every new insert must supply an explicit value.

use async_trait::async_trait;
use sqlx::{Postgres, Transaction};

use super::{Migration, MigrationError};

pub struct AddBusNameToCheckpoints;

#[async_trait]
impl Migration for AddBusNameToCheckpoints {
    fn version(&self) -> i64 {
        8
    }

    fn name(&self) -> &'static str {
        "add_bus_name_to_checkpoints"
    }

    async fn up<'a>(&self, tx: &mut Transaction<'a, Postgres>) -> Result<(), MigrationError> {
        // Add bus_name with a backfill default for existing rows.
        sqlx::query(
            r#"
            ALTER TABLE epoch_event_bus_checkpoints
                ADD COLUMN IF NOT EXISTS bus_name TEXT NOT NULL DEFAULT 'epoch_events'
            "#,
        )
        .execute(&mut **tx)
        .await?;

        // Widen the PK to (bus_name, subscriber_id).
        // Drop the legacy single-column PK constraint first (name may vary after the
        // m004 rename, so we handle both names).
        sqlx::query(
            "ALTER TABLE epoch_event_bus_checkpoints \
             DROP CONSTRAINT IF EXISTS epoch_event_bus_checkpoints_pkey",
        )
        .execute(&mut **tx)
        .await?;
        sqlx::query(
            "ALTER TABLE epoch_event_bus_checkpoints \
             DROP CONSTRAINT IF EXISTS event_bus_checkpoints_pkey",
        )
        .execute(&mut **tx)
        .await?;

        // Add the composite PK (idempotent: if it somehow already exists, this is a no-op
        // at the migration-runner level because this migration only runs once).
        sqlx::query(
            "ALTER TABLE epoch_event_bus_checkpoints \
             ADD PRIMARY KEY (bus_name, subscriber_id)",
        )
        .execute(&mut **tx)
        .await?;

        // Drop the column default now that all rows carry an explicit value.
        sqlx::query(
            "ALTER TABLE epoch_event_bus_checkpoints \
             ALTER COLUMN bus_name DROP DEFAULT",
        )
        .execute(&mut **tx)
        .await?;

        Ok(())
    }
}
