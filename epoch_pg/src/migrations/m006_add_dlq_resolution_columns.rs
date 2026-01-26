//! Migration 006: Add resolution tracking columns to DLQ table.
//!
//! This migration adds columns to track when and how DLQ entries are resolved:
//!
//! - `resolved_at`: Timestamp when the entry was manually resolved
//! - `resolved_by`: Identifier of the operator/system that resolved the entry
//! - `resolution_notes`: Free-form text for operator comments about the resolution
//!
//! These columns help with auditing and debugging production issues by providing
//! a clear history of how failed events were handled.

use async_trait::async_trait;
use sqlx::{Postgres, Transaction};

use super::{Migration, MigrationError};

/// Adds resolution tracking columns to the DLQ table for auditing.
pub struct AddDlqResolutionColumns;

#[async_trait]
impl Migration for AddDlqResolutionColumns {
    fn version(&self) -> i64 {
        6
    }

    fn name(&self) -> &'static str {
        "add_dlq_resolution_columns"
    }

    async fn up<'a>(&self, tx: &mut Transaction<'a, Postgres>) -> Result<(), MigrationError> {
        // Add resolved_at timestamp column
        sqlx::query(
            r#"
            ALTER TABLE epoch_event_bus_dlq
            ADD COLUMN IF NOT EXISTS resolved_at TIMESTAMPTZ
            "#,
        )
        .execute(&mut **tx)
        .await?;

        // Add resolved_by column for operator/system identification
        sqlx::query(
            r#"
            ALTER TABLE epoch_event_bus_dlq
            ADD COLUMN IF NOT EXISTS resolved_by VARCHAR(255)
            "#,
        )
        .execute(&mut **tx)
        .await?;

        // Add resolution_notes column for operator comments
        sqlx::query(
            r#"
            ALTER TABLE epoch_event_bus_dlq
            ADD COLUMN IF NOT EXISTS resolution_notes TEXT
            "#,
        )
        .execute(&mut **tx)
        .await?;

        // Add index on resolved_at to efficiently query unresolved entries
        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_epoch_dlq_resolved_at
            ON epoch_event_bus_dlq(resolved_at)
            WHERE resolved_at IS NULL
            "#,
        )
        .execute(&mut **tx)
        .await?;

        Ok(())
    }
}
