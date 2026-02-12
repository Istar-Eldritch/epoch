//! Migration 007: Add causation and correlation columns to epoch_events.
//!
//! This migration adds `causation_id` and `correlation_id` columns to support
//! event correlation and causation tracking. It also updates the NOTIFY trigger
//! function to include the new fields in the JSON payload.
//!
//! Both columns are nullable — `NULL` means the event predates causation tracking.

use async_trait::async_trait;
use sqlx::{Postgres, Transaction};

use super::{Migration, MigrationError};

/// Adds causation_id and correlation_id columns to epoch_events.
pub struct AddCausationColumns;

#[async_trait]
impl Migration for AddCausationColumns {
    fn version(&self) -> i64 {
        7
    }

    fn name(&self) -> &'static str {
        "add_causation_columns"
    }

    async fn up<'a>(&self, tx: &mut Transaction<'a, Postgres>) -> Result<(), MigrationError> {
        // Add nullable columns (no backfill needed — NULL means "predates tracking")
        sqlx::query(
            r#"
            ALTER TABLE epoch_events ADD COLUMN IF NOT EXISTS causation_id UUID;
            "#,
        )
        .execute(&mut **tx)
        .await?;

        sqlx::query(
            r#"
            ALTER TABLE epoch_events ADD COLUMN IF NOT EXISTS correlation_id UUID;
            "#,
        )
        .execute(&mut **tx)
        .await?;

        // Index for cross-stream correlation queries
        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_epoch_events_correlation_id
            ON epoch_events(correlation_id);
            "#,
        )
        .execute(&mut **tx)
        .await?;

        // Update the NOTIFY trigger function to include new fields
        sqlx::query(
            r#"
            CREATE OR REPLACE FUNCTION epoch_notify_event()
            RETURNS TRIGGER AS $$
            BEGIN
                PERFORM pg_notify(
                    TG_ARGV[0],
                    json_build_object(
                        'id', NEW.id,
                        'stream_id', NEW.stream_id,
                        'stream_version', NEW.stream_version,
                        'event_type', NEW.event_type,
                        'actor_id', NEW.actor_id,
                        'purger_id', NEW.purger_id,
                        'data', NEW.data,
                        'created_at', NEW.created_at,
                        'purged_at', NEW.purged_at,
                        'global_sequence', NEW.global_sequence,
                        'causation_id', NEW.causation_id,
                        'correlation_id', NEW.correlation_id
                    )::text
                );
                RETURN NEW;
            END;
            $$ LANGUAGE plpgsql;
            "#,
        )
        .execute(&mut **tx)
        .await?;

        Ok(())
    }
}
