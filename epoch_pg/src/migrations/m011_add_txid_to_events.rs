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
//! **Table-rename guard (CLOUD-181):** if `epoch_events` does not exist (e.g.
//! after the R28 domain-table cutover that renames it to `epoch_events_legacy`),
//! the migration is a deliberate **no-op** (with a `WARN` log). The legacy table
//! is frozen and not migrated; any post-cutover active table receives `txid` via
//! `event_bus::ensure_txid_column()`.
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
        // Step 0 (CLOUD-181): tolerate the epoch_events table rename.
        //
        // After the R28 domain-table cutover, `epoch_events` is renamed to
        // `epoch_events_legacy`. `ADD COLUMN IF NOT EXISTS` guards the *column*,
        // not the *table*, so without this guard m011 fails with
        // `relation "epoch_events" does not exist` and aborts boot.
        //
        // `to_regclass` with an unqualified name resolves via search_path, exactly
        // like the ALTER/CREATE statements below, so the guard and the DDL agree on
        // which `epoch_events` they mean. When the table is absent the migration is
        // a deliberate no-op: the legacy table is frozen and not migrated, and any
        // post-cutover active table receives `txid` via
        // `event_bus::ensure_txid_column()`.
        let table_exists: bool =
            sqlx::query_scalar("SELECT to_regclass('epoch_events') IS NOT NULL")
                .fetch_one(&mut **tx)
                .await?;

        if !table_exists {
            log::warn!(
                "m011 (add_txid_to_events): table 'epoch_events' not found \
                 (expected after the R28 table-rename cutover); skipping txid \
                 column, default, and index as a no-op."
            );
            return Ok(());
        }

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
