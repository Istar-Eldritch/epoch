//! Migration 013: Create `epoch_snapshots` table.
//!
//! Creates the versioned snapshot store table used by `PgSnapshotStore`.
//! Each row is a `(stream_id, version)` keyed copy of aggregate state serialised
//! as JSONB. The composite primary key makes `save_snapshot` idempotent via
//! `ON CONFLICT DO UPDATE`; the `version DESC` index makes the nearest-`≤` lookup
//! a single index seek.
//!
//! Forward-only — no `down()` (see `migrations::Migration` design note).

use async_trait::async_trait;
use sqlx::{Postgres, Transaction};

use super::{Migration, MigrationError};

/// Migration 013: create `epoch_snapshots`.
pub struct CreateSnapshotsTable;

#[async_trait]
impl Migration for CreateSnapshotsTable {
    fn version(&self) -> i64 {
        13
    }

    fn name(&self) -> &'static str {
        "create_snapshots_table"
    }

    async fn up<'a>(&self, tx: &mut Transaction<'a, Postgres>) -> Result<(), MigrationError> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS epoch_snapshots (
                stream_id  UUID        NOT NULL,
                version    BIGINT      NOT NULL,
                data       JSONB       NOT NULL,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                PRIMARY KEY (stream_id, version)
            )
            "#,
        )
        .execute(&mut **tx)
        .await?;

        // Supports `WHERE stream_id = $1 AND version <= $2 ORDER BY version DESC LIMIT 1`.
        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_epoch_snapshots_stream_version
                ON epoch_snapshots (stream_id, version DESC)
            "#,
        )
        .execute(&mut **tx)
        .await?;

        Ok(())
    }
}
