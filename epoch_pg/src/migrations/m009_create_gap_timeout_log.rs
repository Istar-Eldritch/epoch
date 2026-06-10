//! Migration 009: Create `epoch_event_bus_gap_timeouts` table.
//!
//! Records each global sequence that a subscriber's checkpoint was advanced past
//! because the gap did not fill within `gap_timeout`. One row per
//! `(bus_name, subscriber_id, skipped_sequence)` — idempotent via a `UNIQUE`
//! constraint so re-recording is always safe.
//!
//! The `resolved_at`, `resolved_by`, and `resolution_notes` columns mirror the
//! DLQ resolution workflow introduced in m006, enabling operators to record
//! whether the skipped event was confirmed rolled-back or committed late and
//! replayed via out-of-band means.

use async_trait::async_trait;
use sqlx::{Postgres, Transaction};

use super::{Migration, MigrationError};

/// Migration 009: create `epoch_event_bus_gap_timeouts`.
pub struct CreateGapTimeoutLog;

#[async_trait]
impl Migration for CreateGapTimeoutLog {
    fn version(&self) -> i64 {
        9
    }

    fn name(&self) -> &'static str {
        "create_gap_timeout_log"
    }

    async fn up<'a>(&self, tx: &mut Transaction<'a, Postgres>) -> Result<(), MigrationError> {
        // Main table: one row per (bus, subscriber, skipped sequence).
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS epoch_event_bus_gap_timeouts (
                id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                bus_name         VARCHAR(255) NOT NULL,
                subscriber_id    VARCHAR(255) NOT NULL,
                skipped_sequence BIGINT       NOT NULL,
                gap_duration_ms  BIGINT       NOT NULL,
                timed_out_at     TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
                resolved_at      TIMESTAMPTZ,
                resolved_by      VARCHAR(255),
                resolution_notes TEXT,
                CONSTRAINT uq_epoch_gap_timeouts_bus_sub_seq
                    UNIQUE (bus_name, subscriber_id, skipped_sequence)
            )
            "#,
        )
        .execute(&mut **tx)
        .await?;

        // Index on `skipped_sequence` for range queries (e.g., operators looking
        // for a specific sequence across all subscribers).
        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_epoch_gap_timeouts_sequence
                ON epoch_event_bus_gap_timeouts (skipped_sequence)
            "#,
        )
        .execute(&mut **tx)
        .await?;

        // Partial index on unresolved rows so `resolved_at IS NULL` filters are fast.
        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_epoch_gap_timeouts_unresolved
                ON epoch_event_bus_gap_timeouts (resolved_at)
                WHERE resolved_at IS NULL
            "#,
        )
        .execute(&mut **tx)
        .await?;

        Ok(())
    }
}
