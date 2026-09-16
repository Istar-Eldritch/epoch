//! Migration 014: Create `epoch_events_sequence_counter` table.
//!
//! Backs the opt-in [`AllocationMode::PerTxnCounter`] allocator: one row per
//! events table (keyed by table name, since `PgEventStore`'s events table is
//! configurable), holding the last-assigned `global_sequence` value. Rows are
//! created and seeded by the store constructor, not here — the migration cannot
//! know custom table names.
//!
//! The table is created regardless of the selected allocation mode; it is inert
//! while the default `Nextval` mode is in use.
//!
//! Forward-only — no `down()` (see `migrations::Migration` design note).
//!
//! [`AllocationMode::PerTxnCounter`]: crate::event_store::AllocationMode::PerTxnCounter

use async_trait::async_trait;
use sqlx::{Postgres, Transaction};

use super::{Migration, MigrationError};

/// Migration 014: create `epoch_events_sequence_counter`.
pub struct CreateEventsSequenceCounter;

#[async_trait]
impl Migration for CreateEventsSequenceCounter {
    fn version(&self) -> i64 {
        14
    }

    fn name(&self) -> &'static str {
        "create_events_sequence_counter"
    }

    async fn up<'a>(&self, tx: &mut Transaction<'a, Postgres>) -> Result<(), MigrationError> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS epoch_events_sequence_counter (
                name TEXT   PRIMARY KEY,
                val  BIGINT NOT NULL
            )
            "#,
        )
        .execute(&mut **tx)
        .await?;

        Ok(())
    }
}
