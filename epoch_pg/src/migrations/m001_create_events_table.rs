//! Migration 001: Create the events table.
//!
//! This is the foundational migration that creates the core events table
//! for event sourcing. Uses the legacy name `events` for backwards compatibility
//! with existing production databases. Migration 004 will rename to `epoch_events`.

use async_trait::async_trait;
use sqlx::{Postgres, Transaction};

use super::{Migration, MigrationError};

/// Creates the events table for storing event-sourced data.
pub struct CreateEventsTable;

#[async_trait]
impl Migration for CreateEventsTable {
    fn version(&self) -> i64 {
        1
    }

    fn name(&self) -> &'static str {
        "create_events_table"
    }

    async fn up<'a>(&self, tx: &mut Transaction<'a, Postgres>) -> Result<(), MigrationError> {
        // Use legacy name `events` for backwards compatibility.
        // Existing production databases already have this table.
        // Migration 004 will rename to `epoch_events`.
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS events (
                id UUID PRIMARY KEY,
                stream_id UUID NOT NULL,
                stream_version BIGINT NOT NULL,
                event_type VARCHAR(255) NOT NULL,
                data JSONB,
                created_at TIMESTAMPTZ NOT NULL,
                actor_id UUID,
                purger_id UUID,
                purged_at TIMESTAMPTZ,
                UNIQUE (stream_id, stream_version)
            )
            "#,
        )
        .execute(&mut **tx)
        .await?;

        Ok(())
    }
}
