# Spec 0019-fix: Make `m011_add_txid_to_events` Tolerant of the `epoch_events` Table Rename

**Issue:** Linear CLOUD-181
**Status:** Proposed (ready for review)
**Created:** 2026-06-14
**Crate:** `epoch_pg`
**Commit scope:** `fix(pg)` / concept scope `event-store`
**Builds on:** Spec 0019 (CLOUD-180 snapshot fencing — introduced m011)
**PostgreSQL requirement:** PG 13+ (unchanged; `to_regclass` is available on all supported versions)

---

## 1. Problem Statement

Migration `m011_add_txid_to_events`
(`epoch_pg/src/migrations/m011_add_txid_to_events.rs`) hardcodes the table name
`epoch_events` in three DDL statements:

```sql
ALTER TABLE epoch_events ADD COLUMN IF NOT EXISTS txid BIGINT;
ALTER TABLE epoch_events ALTER COLUMN txid SET DEFAULT (pg_current_xact_id()::text::bigint);
CREATE INDEX IF NOT EXISTS idx_epoch_events_txid ON epoch_events (txid) WHERE txid IS NOT NULL;
```

Production Catacloud has already executed the **R28 domain-table cutover**, which
renames `epoch_events` → `epoch_events_legacy`. The `IF NOT EXISTS` clause on
`ADD COLUMN` guards only the **column**, not the **table**. When
`Migrator::run()` reaches m011 on a cut-over database, `epoch_events` no longer
exists, so the very first statement fails with:

```
relation "epoch_events" does not exist
```

Because each migration runs inside its own transaction, m011's transaction rolls
back. `Migrator::run()` returns `MigrationError::MigrationFailed`, and **web boot
aborts** — the database is permanently stuck one migration short on every
restart.

### Why this is the right layer to fix

The migration is *static SQL embedded in the epoch library*. Deploy-time
workarounds (recreating a stub `epoch_events`, hand-patching the migration table)
are non-durable: any environment that runs migrations after a cutover hits the
same wall. The fix belongs in the migration itself so it is **idempotent
regardless of deploy state** — it must succeed on a fresh DB, a pre-cutover DB,
and a post-cutover DB.

---

## 2. Goals / Non-Goals

### Goals

1. `m011` must **succeed** when `epoch_events` does not exist, becoming a
   no-op (with a `WARN` log) instead of erroring.
2. `m011` must remain **fully functional** on databases where `epoch_events`
   still exists (fresh installs, pre-cutover production): the `txid` column,
   `DEFAULT`, and partial index are created exactly as today.
3. The fix must **not** trigger a `ChecksumMismatch` on databases where m011 has
   already been applied successfully.
4. The behaviour must be verified by an automated migration integration test
   that reproduces the post-cutover state.

### Non-Goals

- **Migrating the legacy table.** `epoch_events_legacy` is a frozen,
  domain-owned artifact of the Catacloud R28 cutover. epoch does not add `txid`
  to it. (Option 3 from the ticket is explicitly rejected.)
- **Creating a stub `epoch_events`.** (Option 2 from the ticket is rejected as
  non-durable.)
- **Reconciling what the post-cutover active events table is.** That is a
  Catacloud concern; custom events tables already receive `txid` via
  `event_bus::ensure_txid_column()` (called by `PgEventStore::with_table()`).
- Any change to migration ordering, the `Migration` trait, or `Migrator::run()`.

---

## 3. Design

### 3.1 Approach (Option 1: make m011 table-aware)

Add a single existence guard at the top of `AddTxidToEvents::up()`. If
`epoch_events` is absent, log a `WARN` and return `Ok(())` immediately. This
mirrors the established precedent in
`m004_rename_tables_with_epoch_prefix.rs`, which guards every rename with an
existence check to stay idempotent across pre- and post-rename states.

We use `to_regclass()` rather than an `information_schema.tables` query because:

- It is a single scalar expression (no row decoding boilerplate).
- It is **search-path relative** when given an unqualified name, exactly
  matching how the migration's own `ALTER TABLE epoch_events` statements resolve
  the table. This keeps the guard correct even if epoch is installed under a
  non-`public` schema (the unqualified DDL and the guard resolve identically).
- It returns `NULL` for a missing relation, so `IS NOT NULL` is an unambiguous
  boolean.

### 3.2 Checksum safety

`Migration::checksum()` (default impl in `migrations/mod.rs`) hashes **only**
`version + name`, *not* the SQL body. Editing m011's `up()` body therefore does
**not** change its checksum. Consequences:

- Databases where m011 already applied successfully: checksum still matches → no
  `ChecksumMismatch`, and m011 is **not** re-run (it is in `applied_versions`).
  These DBs are unaffected.
- Databases where m011 has never been recorded as applied (the failing
  post-cutover case): m011 is still pending and will be re-run with the fixed,
  guard-protected body → it now succeeds and is recorded.

No checksum override is added; the default behaviour is exactly what we want.

### 3.3 Code change

**File:** `epoch_pg/src/migrations/m011_add_txid_to_events.rs`

Add the guard as a new first step in `up()`; the three existing statements are
unchanged and become steps 2–4, only reached when the table exists.

```rust
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
    sqlx::query(
        r#"
        ALTER TABLE epoch_events
            ADD COLUMN IF NOT EXISTS txid BIGINT
        "#,
    )
    .execute(&mut **tx)
    .await?;

    // Step 2: set the column DEFAULT for future inserts.
    sqlx::query(
        r#"
        ALTER TABLE epoch_events
            ALTER COLUMN txid SET DEFAULT (pg_current_xact_id()::text::bigint)
        "#,
    )
    .execute(&mut **tx)
    .await?;

    // Step 3: partial index for forensic / high-water lookups on committed rows.
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
```

The module-level doc comment is updated with a one-line note that the migration
is a no-op when `epoch_events` is absent (post-cutover), to keep the rustdoc
honest.

No new dependencies. `log` is already a dependency used throughout `epoch_pg`.

---

## 4. Behavioural Matrix

| Database state                                   | m011 status before | Behaviour after fix                                     |
|--------------------------------------------------|--------------------|---------------------------------------------------------|
| Fresh install (`epoch_events` exists)            | pending            | Adds column + default + index (unchanged)               |
| Pre-cutover prod (`epoch_events` exists)         | pending            | Adds column + default + index (unchanged)               |
| Post-cutover prod (`epoch_events_legacy` only)   | pending            | **No-op + WARN**, recorded as applied → boot succeeds   |
| Any DB where m011 already applied                | applied            | Skipped by `applied_versions` (unchanged, no re-run)    |

---

## 5. Testing Plan (TDD)

All tests go in `epoch_pg/tests/migrations_integration_tests.rs`, which already
runs against the dedicated `epoch_pg_test_migrations` database and uses
`teardown()` for isolation. Each test is gated on
`common::try_get_pg_pool_for_db(...)` returning `Some`, matching existing
conventions (skips cleanly when no DB is available).

### Step 1 — Failing test: m011 is a no-op after the table rename

Add `test_m011_skips_when_epoch_events_renamed`:

1. `teardown(&pool)`.
2. `Migrator::run()` to apply all migrations (m011 included → `epoch_events`
   has `txid`).
3. Simulate a database that has cut over **but not yet applied m011**:
   - `DROP INDEX IF EXISTS idx_epoch_events_txid;`
   - `ALTER TABLE epoch_events DROP COLUMN IF EXISTS txid;`
   - `ALTER TABLE epoch_events RENAME TO epoch_events_legacy;`
   - `DELETE FROM _epoch_migrations WHERE version = 11;`
4. `Migrator::run()` again → **assert it returns `Ok`** (this is the assertion
   that fails against the current hardcoded migration with
   `relation "epoch_events" does not exist`).
5. Assert m011 is re-recorded: `applied()` contains version 11.
6. Assert no `epoch_events` table was recreated
   (`to_regclass('epoch_events') IS NULL`) and `epoch_events_legacy` still
   exists.
7. Cleanup: `DROP TABLE IF EXISTS epoch_events_legacy CASCADE;` then
   `teardown(&pool)`.

> Note: `teardown()` only drops `epoch_events`; the test must drop
> `epoch_events_legacy` itself (or rename it back) so it does not leak into other
> serial tests.

### Step 2 — Implement the guard

Apply the §3.3 change. Re-run the test → it passes.

### Step 3 — Regression test: normal path still adds the column

Add (or extend an existing run-through test with)
`test_m011_adds_txid_column_when_table_present`:

1. `teardown`, `Migrator::run()`.
2. Assert the column exists:
   ```sql
   SELECT EXISTS (
     SELECT 1 FROM information_schema.columns
     WHERE table_name = 'epoch_events' AND column_name = 'txid'
   )
   ```
3. Assert the partial index `idx_epoch_events_txid` exists
   (`pg_indexes.indexname`).
4. `teardown`.

### Step 4 — Idempotency

Confirm the existing full-run / "run twice" coverage still passes (m011 in
`applied_versions` is skipped on the second `run()`), and that
`test_migrator_applies_all_migrations` (asserting
`applied_after[10].name == "add_txid_to_events"` and
`applied.len() == MIGRATION_COUNT`) is unaffected.

### Verification commands

```bash
cargo fmt
cargo clippy -p epoch_pg -- -D warnings
cargo test -p epoch_pg --test migrations_integration_tests
```

---

## 6. Rollout

1. Merge the epoch fix; cut a patch release of `epoch_pg`.
2. Bump Catacloud's `epoch_pg` dependency to the patched version.
3. On next web boot against the cut-over database, `Migrator::run()` applies
   m011 as a no-op, records version 11, and boot proceeds. No manual DB
   intervention required.

No backfill, no data migration, no downtime window beyond the normal deploy.

---

## 7. Risks & Mitigations

| Risk                                                                 | Mitigation                                                                                          |
|----------------------------------------------------------------------|----------------------------------------------------------------------------------------------------|
| `to_regclass` resolves a *different* `epoch_events` than the DDL      | Both use the same unqualified, search-path-relative name → identical resolution.                    |
| A future cutover recreates `epoch_events` *without* `txid`            | Re-running migrations is gated by `applied_versions`; if a fresh table genuinely needs `txid`, use `event_bus::ensure_txid_column()` (already wired for custom tables) or a new forward migration. |
| Silent no-op hides a real misconfiguration                           | The no-op emits a `WARN` naming the table and the expected cutover cause, so it is observable in logs. |
| Checksum drift breaks already-migrated DBs                            | Default checksum hashes only version+name; SQL body edits don't change it (§3.2).                   |

---

## 8. Summary of Changes

- **`epoch_pg/src/migrations/m011_add_txid_to_events.rs`** — add a
  `to_regclass('epoch_events')` existence guard at the top of `up()`; no-op +
  `WARN` when absent; update module doc comment.
- **`epoch_pg/tests/migrations_integration_tests.rs`** — add
  `test_m011_skips_when_epoch_events_renamed` (post-cutover no-op) and a
  column/index presence assertion for the normal path.
- No dependency, trait, ordering, or checksum changes.
