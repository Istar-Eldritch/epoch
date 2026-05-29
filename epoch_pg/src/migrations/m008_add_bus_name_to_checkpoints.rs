//! Migration 008: Add `bus_name` column to `epoch_event_bus_checkpoints`.
//!
//! Required for the composite checkpoint key `(bus_name, subscriber_id)` so that
//! per-domain buses (each with their own `events_table`) maintain isolated
//! checkpoint rows in the shared table.
//! Existing rows are backfilled to `'epoch_events'` (the legacy default).

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
        sqlx::query(
            r#"
            ALTER TABLE epoch_event_bus_checkpoints
                ADD COLUMN IF NOT EXISTS bus_name TEXT NOT NULL DEFAULT 'epoch_events'
            "#,
        )
        .execute(&mut **tx)
        .await?;

        Ok(())
    }
}
