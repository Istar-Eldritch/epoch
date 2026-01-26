//! Migration 005: Add index on checkpoint last_global_sequence.
//!
//! This migration adds an index on the `last_global_sequence` column in the
//! checkpoints table. This index enables efficient queries to find the minimum
//! checkpoint across all subscribers, which is useful for:
//!
//! - Event retention decisions (knowing which events can be safely archived)
//! - Event purging (determining the safe purge boundary)
//! - Monitoring subscriber lag

use async_trait::async_trait;
use sqlx::{Postgres, Transaction};

use super::{Migration, MigrationError};

/// Adds an index on checkpoint last_global_sequence for efficient min-checkpoint queries.
pub struct AddCheckpointSequenceIndex;

#[async_trait]
impl Migration for AddCheckpointSequenceIndex {
    fn version(&self) -> i64 {
        5
    }

    fn name(&self) -> &'static str {
        "add_checkpoint_sequence_index"
    }

    async fn up<'a>(&self, tx: &mut Transaction<'a, Postgres>) -> Result<(), MigrationError> {
        // Add index on last_global_sequence for efficient queries that find
        // the minimum checkpoint across all subscribers
        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_epoch_checkpoints_global_sequence
            ON epoch_event_bus_checkpoints(last_global_sequence)
            "#,
        )
        .execute(&mut **tx)
        .await?;

        Ok(())
    }
}
