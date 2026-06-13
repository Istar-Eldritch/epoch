//! Migration 011: Add `txid BIGINT` column to `epoch_events`.
//!
//! This migration adds a nullable `txid BIGINT` column that is automatically
//! populated on every new INSERT via a PostgreSQL column `DEFAULT` expression:
//!
//! ```sql
//! DEFAULT (pg_current_xact_id()::text::bigint)
//! ```
//!
//! `pg_current_xact_id()` is a PG13+ function returning an epoch-extended
//! 64-bit transaction id (`xid8`). Casting through `text` avoids the implicit
//! `xid8 → int8` truncation that was removed in PG16. The resulting `BIGINT`
//! value never wraps within a cluster's lifetime (unlike raw `xid`).
//!
//! **No backfill** is performed: rows written before this migration keep
//! `txid = NULL` and are treated as globally committed by the snapshot fence
//! (CLOUD-180, spec 0019).
//!
//! A partial index on non-NULL `txid` values supports forensic lookups
//! (correlating backstop-skipped sequences to their writing transaction) without
//! adding per-row overhead on the common reader path.
//!
//! **Requirements:** PostgreSQL 13+.

use async_trait::async_trait;
use sqlx::{Postgres, Transaction};

use super::{Migration, MigrationError};

/// Migration 011: add nullable `txid BIGINT` with PG13+ DEFAULT to `epoch_events`.
pub struct AddTxidToEvents;

#[async_trait]
impl Migration for AddTxidToEvents {
    fn version(&self) -> i64 {
        11
    }

    fn name(&self) -> &'static str {
        "add_txid_to_events"
    }

    async fn up<'a>(&self, tx: &mut Transaction<'a, Postgres>) -> Result<(), MigrationError> {
        // Step 1: add the nullable column (metadata-only; no table rewrite).
        // Existing rows receive txid = NULL (NG-5 / §7.1 of spec 0019).
        sqlx::query(
            r#"
            ALTER TABLE epoch_events
                ADD COLUMN IF NOT EXISTS txid BIGINT
            "#,
        )
        .execute(&mut **tx)
        .await?;

        // Step 2: set the column DEFAULT for future inserts.
        // pg_current_xact_id() is volatile, so it is evaluated per row at INSERT
        // time, stamping each new event with its inserting transaction's id.
        // The cast chain ::text::bigint is required on PG16+ where the direct
        // xid8→int8 cast was removed; it is safe on PG13–15 as well.
        sqlx::query(
            r#"
            ALTER TABLE epoch_events
                ALTER COLUMN txid SET DEFAULT (pg_current_xact_id()::text::bigint)
            "#,
        )
        .execute(&mut **tx)
        .await?;

        // Step 3: partial index for forensic / high-water lookups on committed rows.
        // Partial (WHERE txid IS NOT NULL) so pre-migration NULL rows are excluded,
        // keeping the index small.
        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_epoch_events_txid
                ON epoch_events (txid) WHERE txid IS NOT NULL
            "#,
        )
        .execute(&mut **tx)
        .await?;

        Ok(())
    }
}
