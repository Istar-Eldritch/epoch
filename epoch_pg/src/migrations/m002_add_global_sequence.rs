//! Migration 002: Add global sequence to events table.
//!
//! This migration adds a global sequence number to events for ordering
//! across all streams. It also backfills existing events and adds a NOT NULL
//! constraint after backfill completes.
//! Uses legacy table name `events` for backwards compatibility.
//! Migration 004 will rename to `epoch_events`.

use async_trait::async_trait;
use sqlx::{Postgres, Transaction};

use super::{Migration, MigrationError};

/// Adds global_sequence column and sequence to the events table.
pub struct AddGlobalSequence;

#[async_trait]
impl Migration for AddGlobalSequence {
    fn version(&self) -> i64 {
        2
    }

    fn name(&self) -> &'static str {
        "add_global_sequence"
    }

    async fn up<'a>(&self, tx: &mut Transaction<'a, Postgres>) -> Result<(), MigrationError> {
        // Create the global sequence (legacy name, will be renamed in migration 004)
        sqlx::query(
            r#"
            CREATE SEQUENCE IF NOT EXISTS events_global_sequence_seq
            "#,
        )
        .execute(&mut **tx)
        .await?;

        // Add the global_sequence column with default
        sqlx::query(
            r#"
            ALTER TABLE events
            ADD COLUMN IF NOT EXISTS global_sequence BIGINT
            DEFAULT nextval('events_global_sequence_seq')
            "#,
        )
        .execute(&mut **tx)
        .await?;

        // Backfill existing rows that don't have a global_sequence
        // Uses a cursor-based approach to ensure deterministic sequence assignment
        // by processing rows one-by-one in the desired order (created_at, then id).
        sqlx::query(
            r#"
            DO $$
            DECLARE
                event_record RECORD;
            BEGIN
                FOR event_record IN
                    SELECT id
                    FROM events
                    WHERE global_sequence IS NULL
                    ORDER BY created_at, id
                    FOR UPDATE
                LOOP
                    UPDATE events
                    SET global_sequence = nextval('events_global_sequence_seq')
                    WHERE id = event_record.id;
                END LOOP;
            END $$;
            "#,
        )
        .execute(&mut **tx)
        .await?;

        // Add NOT NULL constraint after backfill completes
        // This ensures data integrity for all future events
        sqlx::query(
            r#"
            ALTER TABLE events
            ALTER COLUMN global_sequence SET NOT NULL
            "#,
        )
        .execute(&mut **tx)
        .await?;

        // Create index for efficient ordering and range queries
        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_events_global_sequence
            ON events(global_sequence)
            "#,
        )
        .execute(&mut **tx)
        .await?;

        // Set sequence ownership to the column (helps with pg_dump)
        sqlx::query(
            r#"
            ALTER SEQUENCE events_global_sequence_seq
            OWNED BY events.global_sequence
            "#,
        )
        .execute(&mut **tx)
        .await?;

        Ok(())
    }
}
