//! Migration 003: Create event bus infrastructure.
//!
//! This migration creates the supporting infrastructure for the event bus:
//! - Notification function for LISTEN/NOTIFY
//! - Checkpoint table for tracking subscriber progress
//! - Dead letter queue for failed event processing
//!
//! Uses legacy table names for backwards compatibility.
//! Migration 004 will rename to use `epoch_` prefix.

use async_trait::async_trait;
use sqlx::{Postgres, Transaction};

use super::{Migration, MigrationError};

/// Creates event bus infrastructure (notification function, checkpoints, DLQ).
pub struct CreateEventBusInfrastructure;

#[async_trait]
impl Migration for CreateEventBusInfrastructure {
    fn version(&self) -> i64 {
        3
    }

    fn name(&self) -> &'static str {
        "create_event_bus_infrastructure"
    }

    async fn up<'a>(&self, tx: &mut Transaction<'a, Postgres>) -> Result<(), MigrationError> {
        // Create the notification function used by triggers (legacy name)
        sqlx::query(
            r#"
            CREATE OR REPLACE FUNCTION epoch_pg_notify_event()
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
                        'global_sequence', NEW.global_sequence
                    )::text
                );
                RETURN NEW;
            END;
            $$ LANGUAGE plpgsql
            "#,
        )
        .execute(&mut **tx)
        .await?;

        // Create checkpoint table for tracking subscriber progress (legacy name)
        // Note: Index on last_global_sequence is added in migration 005.
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS event_bus_checkpoints (
                subscriber_id VARCHAR(255) PRIMARY KEY,
                last_global_sequence BIGINT NOT NULL DEFAULT 0,
                last_event_id UUID,
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )
            "#,
        )
        .execute(&mut **tx)
        .await?;

        // Create dead letter queue for failed event processing (legacy name)
        // Note: Resolution tracking columns (resolved_at, resolved_by, resolution_notes)
        // are added in migration m006_add_dlq_resolution_columns.
        //
        // Design Decision: No foreign key to epoch_events(id) for event_id.
        // While a FK would ensure referential integrity, we intentionally omit it:
        // 1. Events should never be deleted in event-sourced systems
        // 2. DLQ entries serve as audit records that should survive event compaction
        // 3. FK constraints add overhead to DLQ inserts (which happen under failure conditions)
        // 4. Allows DLQ to reference events in external stores (future extensibility)
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS event_bus_dlq (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                subscriber_id VARCHAR(255) NOT NULL,
                event_id UUID NOT NULL,
                global_sequence BIGINT NOT NULL,
                error_message TEXT,
                retry_count INT NOT NULL DEFAULT 0,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                last_retry_at TIMESTAMPTZ,
                CONSTRAINT unique_subscriber_event UNIQUE (subscriber_id, event_id)
            )
            "#,
        )
        .execute(&mut **tx)
        .await?;

        // Create indexes for efficient DLQ queries
        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_dlq_subscriber
            ON event_bus_dlq(subscriber_id)
            "#,
        )
        .execute(&mut **tx)
        .await?;

        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_dlq_created_at
            ON event_bus_dlq(created_at)
            "#,
        )
        .execute(&mut **tx)
        .await?;

        Ok(())
    }
}
