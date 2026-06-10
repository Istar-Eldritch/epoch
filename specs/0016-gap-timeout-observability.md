# Spec 0016: Gap-Timeout Observability + Snapshot Fencing Investigation

**Issue:** Linear CLOUD-169
**Status:** Proposed (ready for review)
**Created:** 2026-06-10
**Crate:** `epoch_pg`
**Commit scope:** `feat(pg)` / concept scope `event-bus`

---

## 1. Problem Statement

`PgEventBus` uses a gap-resolution heuristic (`gap_timeout`, default 5 s, defined on
`ReliableDeliveryConfig` in `epoch_pg/src/event_bus/config.rs`) to handle gaps in
`global_sequence`. When a gap is older than `gap_timeout`, the system assumes the
inserting transaction was rolled back and advances the checkpoint past the missing
sequence (`advance_contiguous_checkpoint` in
`epoch_pg/src/event_bus/subscriber_state.rs`).

If the transaction was actually still in-flight — slow batch import, lock contention,
or a long `AggregateTransaction` holding many `handle()` calls open (made more likely
by spec 0007 transaction support) — the committed event is **permanently skipped**,
with:

- Only an `INFO`-level log line (`log::info!("Advancing past gap at seq {} ...")`),
  easy to miss and lacking subscriber context
- No durable record of what was skipped
- No counter, metric hook, or callback to alert operators
- No way for operators to detect or replay the skipped range after the fact

This is a silent data-loss mode in the reliability layer. Because the checkpoint
advances past the gap, the late-committing event is never delivered to that
subscriber, even though it is durably present in `epoch_events`.

---

## 2. Scope

| Priority | Work |
|----------|------|
| **Must** | WARN log + structured `on_gap_timeout` callback + durable `epoch_event_bus_gap_timeouts` table (migration m009) + query API |
| **Should** | Operator resolution API (`resolve_gap_timeout`) mirroring the DLQ resolution-column workflow |
| **Should** | Investigate and document snapshot fencing (`pg_current_snapshot()`) as a future mitigation; determine whether adding a `txid` column to `epoch_events` is warranted (§8) |
| **Deferred** | Full snapshot fencing implementation (separate spec/issue once observability data is available) |

A skipped sequence is recorded **per subscriber**: each subscriber advances its own
checkpoint independently, so the same gap may produce one record per subscriber. This
is intentional — replay/resolution is a per-subscriber concern. Operators can derive
ranges by querying contiguous `skipped_sequence` values.

---

## 3. Codebase Grounding (verified 2026-06-10)

| Fact | Location | Implication |
|------|----------|-------------|
| Gap timeout fires `log::info!("Advancing past gap at seq {} (timed out after {:?})", ...)` inside the advancement loop | `subscriber_state.rs`, `advance_contiguous_checkpoint` (~line 105) | Remove the log from the pure function; report skips to the caller instead |
| `advance_contiguous_checkpoint(state, visible_seqs, gap_timeout)` returns `()` and is a pure, synchronous, DB-free function | `subscriber_state.rs:76` | Change return type to `Vec<SkippedGap>`; keep the function pure (no async, no DB) |
| Sole production call site is `process_subscriber_for_batch` | `mod.rs:157` | All async side-effects (WARN log, DB write, callback) live here; the function already owns `config`, `checkpoint_pool`, and `subscriber_id` |
| `BatchContext` carries `config: ReliableDeliveryConfig`, `dlq_pool: PgPool`, `checkpoint_pool: PgPool` | `mod.rs:50–58` | No new plumbing needed to reach a pool and the config at the gap-timeout site |
| Checkpoint rows are keyed by `(bus_name, subscriber_id)` and `bus_name` is bound from `config.events_table` | `checkpoint.rs:76–91`, `flush_checkpoint(..., &config.events_table, ...)` calls in `mod.rs` | `GapTimeoutInfo.bus_name` and the `bus_name` column must be populated from `config.events_table` for consistency with checkpoints |
| `on_dlq_insertion: Option<Arc<dyn DlqCallback>>` + `DlqInsertionInfo` struct; callback invoked **after** successful DLQ persistence; errors logged, never affect checkpointing | `config.rs:8–53`, `retry.rs:161–200` | Exact pattern to mirror for `on_gap_timeout` / `GapTimeoutInfo` / `GapTimeoutCallback` |
| DLQ insert is fire-and-forget (failure logged via `error!`, processing continues) | `retry.rs` | Same contract for the gap-timeout DB write |
| `ReliableDeliveryConfig` has a manual `Debug` impl rendering the DLQ callback as `Some("Some(<callback>)")` / `None` | `config.rs` | Add the new field to the `Debug` impl the same way |
| Existing customization test constructs `ReliableDeliveryConfig` with **all fields listed** | `config.rs` test `reliable_delivery_config_can_be_customized` | Adding a field requires updating this test (it does not use `..Default::default()`) |
| Next free migration slot is version 9; `MIGRATIONS` array in `migrations/mod.rs` | `migrations/mod.rs` (m001–m008 registered) | New file `m009_create_gap_timeout_log.rs`, version `9` |
| `epoch_event_bus_dlq` gained `resolved_at`, `resolved_by`, `resolution_notes` + partial index on unresolved rows in m006 | `m006_add_dlq_resolution_columns.rs` | Mirror these columns/index on the new table for the same operator workflow |
| Existing DLQ query APIs (`get_dlq_entries`, `get_dlq_entries_paginated`, `count_dlq_entries`, `remove_dlq_entry`) return `Result<_, SqlxError>` | `mod.rs:1160–1300` | New query APIs use `Result<_, SqlxError>` for consistency (not `PgEventBusError`) |
| `gap_first_seen: HashMap<u64, Instant>` uses `tokio::time::Instant` (monotonic, no wall-clock conversion) | `subscriber_state.rs:39` | Store `gap_duration` (computed via `first_seen.elapsed()` at timeout) in `SkippedGap`; the table records `timed_out_at DEFAULT NOW()` at insert time |
| Once a gap times out it is removed from `gap_first_seen` and the checkpoint advances, so a given `(bus, subscriber, sequence)` skip fires at most once per process lifetime; after restart the checkpoint is already past it | `subscriber_state.rs` advancement loop | A `UNIQUE (bus_name, subscriber_id, skipped_sequence)` constraint + `ON CONFLICT DO NOTHING` makes the insert idempotent against edge-case re-emission |
| Public re-exports: `epoch_pg/src/lib.rs:60–62` re-exports event-bus config types; the `epoch` facade crate re-exports `epoch_pg::*` via `pg_store` | `epoch_pg/src/lib.rs`, `epoch/src/lib.rs:19–20` | Adding the new types to `epoch_pg`'s `pub use event_bus::{...}` automatically surfaces them through the facade |
| Integration test harness uses `serial_test::serial`, `#[tokio::test]`, a shared `setup()` helper, and fresh `Uuid::new_v4()` stream IDs | `epoch_pg/tests/pgeventbus_integration_tests.rs` | New integration tests follow the same conventions |

---

## 4. Design Overview

```
advance_contiguous_checkpoint (pure, sync)          process_subscriber_for_batch (async)
┌─────────────────────────────────────┐             ┌──────────────────────────────────────┐
│ gap at seq N older than gap_timeout │──returns──▶ │ for each SkippedGap:                 │
│ → advance checkpoint past N         │  Vec<       │   1. log::warn! (bus, sub, seq, dur) │
│ → push SkippedGap { N, duration }   │  SkippedGap>│   2. tokio::spawn (fire-and-forget): │
└─────────────────────────────────────┘             │      a. INSERT INTO                  │
                                                    │         epoch_event_bus_gap_timeouts │
                                                    │         (ON CONFLICT DO NOTHING)     │
                                                    │      b. on insert OK → invoke        │
                                                    │         on_gap_timeout callback      │
                                                    └──────────────────────────────────────┘
```

Division of responsibility:

- **`subscriber_state.rs` stays pure.** It detects and decides; it does not log at
  WARN, write to the DB, or invoke callbacks. It returns what it skipped.
- **`mod.rs` performs all side-effects.** It owns the subscriber ID, bus name
  (`config.events_table`), pools, and config, and already runs in an async context.
- **Recording never gates advancement.** The checkpoint advance is the existing
  behavior and is unchanged; observability is strictly additive and detached.

---

## 5. Functional Requirements

Each requirement is independently verifiable; verification is listed inline.

### FR-1 — `SkippedGap` return value from `advance_contiguous_checkpoint`

Change the signature in `subscriber_state.rs` from:

```rust
pub(crate) fn advance_contiguous_checkpoint(
    state: &mut SubscriberState,
    visible_seqs: &BTreeSet<u64>,
    gap_timeout: Duration,
)
```

to return `Vec<SkippedGap>`, where:

```rust
/// Describes a single gap that was advanced past due to timeout.
/// One entry is returned for each sequence the checkpoint skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkippedGap {
    /// The `global_sequence` that was skipped.
    pub skipped_sequence: u64,
    /// How long the gap was observed before the timeout fired
    /// (`first_seen.elapsed()` at the moment of the skip; always > `gap_timeout`).
    pub gap_duration: Duration,
}
```

The `log::info!` currently emitted at the timeout branch is **removed** from this
function (the caller logs at WARN per FR-2; a `log::debug!` may optionally remain).
The function remains pure, synchronous, and DB-free (NFR-3).

*Verify:* unit tests in `subscriber_state.rs` (§10) assert returned contents; the
empty-`Vec` case for non-timed-out paths; `cargo test -p epoch_pg` compiles all call
sites against the new signature.

### FR-2 — WARN-level log at the call site

In `process_subscriber_for_batch` (`mod.rs`), immediately after
`advance_contiguous_checkpoint` returns, emit one `log::warn!` per `SkippedGap`
including, at minimum: the bus name (`config.events_table`), the `subscriber_id`,
the `skipped_sequence`, and the `gap_duration`. Example shape:

```
WARN Gap timeout: advancing '<subscriber_id>' on bus '<bus>' past missing seq <N> \
     after <dur> — if the writing transaction later commits, the event will NOT be \
     delivered to this subscriber (recorded in epoch_event_bus_gap_timeouts)
```

*Verify:* code review + the existing `log` facade; integration test may assert via a
test logger, but the durable record (FR-6/FR-7) is the primary machine check.

### FR-3 — `GapTimeoutInfo` struct

New public struct in `config.rs`, mirroring `DlqInsertionInfo`:

```rust
/// Information about a gap that was advanced past due to timeout.
/// Passed to the [`GapTimeoutCallback`].
#[derive(Debug, Clone)]
pub struct GapTimeoutInfo {
    /// The bus on which the gap occurred (the configured `events_table`,
    /// consistent with the `bus_name` used by checkpoints).
    pub bus_name: String,
    /// The subscriber whose checkpoint advanced past the gap.
    pub subscriber_id: String,
    /// The `global_sequence` that was skipped.
    pub skipped_sequence: u64,
    /// How long the gap was observed before the timeout fired.
    pub gap_duration: Duration,
}
```

*Verify:* unit test `gap_timeout_info_fields` (§10).

### FR-4 — `GapTimeoutCallback` trait

New public async trait in `config.rs`, mirroring `DlqCallback`:

```rust
/// Callback invoked when a subscriber's checkpoint is advanced past a gap
/// due to timeout.
///
/// Use this to increment metrics counters or trigger alerts. Implementations
/// should be lightweight. The callback fires after the gap-timeout record has
/// been persisted; errors or panics inside the callback are isolated in a
/// detached task and do **not** affect checkpoint advancement — the gap is
/// always skipped regardless of callback outcome.
#[async_trait]
pub trait GapTimeoutCallback: Send + Sync {
    /// Called after a gap-timeout record has been persisted.
    async fn on_gap_timeout(&self, info: GapTimeoutInfo);
}
```

*Verify:* unit test `gap_timeout_callback_can_be_set` + integration test
`test_gap_timeout_callback_is_invoked` (§10).

### FR-5 — `on_gap_timeout` field on `ReliableDeliveryConfig`

Add to `ReliableDeliveryConfig` in `config.rs`:

```rust
/// Optional callback invoked when a gap is advanced past due to timeout.
///
/// Fires after the gap-timeout record has been persisted to
/// `epoch_event_bus_gap_timeouts`. Errors from the callback are logged but do
/// not affect checkpoint advancement.
///
/// Default: `None` (no callback)
pub on_gap_timeout: Option<Arc<dyn GapTimeoutCallback>>,
```

Requirements:

1. `Default` impl sets it to `None`.
2. The manual `Debug` impl renders it as `Some("Some(<callback>)")` / `None`,
   identical to `on_dlq_insertion`.
3. The existing exhaustive-construction test
   `reliable_delivery_config_can_be_customized` is updated to include the field.

*Verify:* unit tests `on_gap_timeout_defaults_to_none`,
`gap_timeout_callback_can_be_set`, and the updated `Debug` test (§10).

### FR-6 — Migration m009: `epoch_event_bus_gap_timeouts` table

New file `epoch_pg/src/migrations/m009_create_gap_timeout_log.rs` implementing
`Migration` with `version() == 9`, `name() == "create_gap_timeout_log"`, registered
at the end of the `MIGRATIONS` array in `migrations/mod.rs`:

```sql
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
);

CREATE INDEX IF NOT EXISTS idx_epoch_gap_timeouts_sequence
    ON epoch_event_bus_gap_timeouts (skipped_sequence);

CREATE INDEX IF NOT EXISTS idx_epoch_gap_timeouts_unresolved
    ON epoch_event_bus_gap_timeouts (resolved_at)
    WHERE resolved_at IS NULL;
```

Notes:

- The `UNIQUE` constraint makes the recording insert idempotent (`ON CONFLICT DO
  NOTHING`, FR-7) and doubles as the lookup index for `(bus_name, subscriber_id)`
  queries, so no separate composite index is needed.
- `resolved_at` / `resolved_by` / `resolution_notes` mirror the DLQ pattern (m006),
  giving operators an in-database workflow for recording whether the skipped event
  was confirmed rolled back (no action) or was committed late and replayed.
- Forward-only, consistent with the migrator's no-rollback design.

*Verify:* the existing migration meta-tests (`migrations_are_in_order`,
`all_migrations_have_unique_versions/names`) pass; integration `setup()` applies m009
and the table exists.

### FR-7 — Durable persistence + callback on gap timeout

In `process_subscriber_for_batch` (`mod.rs`), for each `SkippedGap` returned by
`advance_contiguous_checkpoint`:

1. Emit the WARN log (FR-2).
2. Spawn a detached `tokio::spawn` task (fire-and-forget) that:
   a. Executes
      ```sql
      INSERT INTO epoch_event_bus_gap_timeouts
          (bus_name, subscriber_id, skipped_sequence, gap_duration_ms)
      VALUES ($1, $2, $3, $4)
      ON CONFLICT (bus_name, subscriber_id, skipped_sequence) DO NOTHING
      ```
      binding `bus_name = config.events_table`, the subscriber ID, the skipped
      sequence as `i64`, and `gap_duration.as_millis() as i64`, using the
      `checkpoint_pool` already available in scope.
   b. On insert error: `log::error!` and return (no retry, no panic).
   c. On insert success (including conflict/no-op): if `config.on_gap_timeout` is
      `Some(cb)`, build `GapTimeoutInfo` and `cb.on_gap_timeout(info).await`.
3. Continue batch processing without awaiting the spawned task. Checkpoint
   advancement and flushing are not conditioned on the recording outcome (NFR-1).

*Verify:* integration tests `test_gap_timeout_inserts_record` and
`test_gap_timeout_callback_is_invoked` (§10).

### FR-8 — `GapTimeoutEntry` struct + `list_gap_timeouts` query API

New public struct in `mod.rs` (next to `DlqEntry`) exposing all columns:

```rust
/// A recorded gap-timeout: a global sequence a subscriber's checkpoint
/// advanced past because the gap did not fill within `gap_timeout`.
#[derive(Debug, Clone)]
pub struct GapTimeoutEntry {
    /// Unique identifier for the record.
    pub id: Uuid,
    /// The bus (events table) on which the gap occurred.
    pub bus_name: String,
    /// The subscriber whose checkpoint advanced past the gap.
    pub subscriber_id: String,
    /// The `global_sequence` that was skipped.
    pub skipped_sequence: u64,
    /// How long the gap was observed before the timeout fired, in milliseconds.
    pub gap_duration_ms: i64,
    /// When the skip was recorded.
    pub timed_out_at: chrono::DateTime<chrono::Utc>,
    /// When the record was manually resolved (None if unresolved).
    pub resolved_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Identifier of the operator/system that resolved the record.
    pub resolved_by: Option<String>,
    /// Free-form notes about the resolution.
    pub resolution_notes: Option<String>,
}
```

New public method on `PgEventBus`, scoped to the bus's own `bus_name`
(`config.events_table`) and following the existing DLQ query API conventions
(`Result<_, SqlxError>`, ordered oldest-first, paginated):

```rust
/// Lists gap-timeout records for this bus.
///
/// # Arguments
/// * `subscriber_id` - Restrict to one subscriber, or `None` for all
/// * `unresolved_only` - When `true`, only records with `resolved_at IS NULL`
/// * `offset` / `limit` - Pagination (ordered by `timed_out_at` ascending)
pub async fn list_gap_timeouts(
    &self,
    subscriber_id: Option<&str>,
    unresolved_only: bool,
    offset: u64,
    limit: u64,
) -> Result<Vec<GapTimeoutEntry>, SqlxError>
```

*Verify:* integration test `test_list_gap_timeouts_returns_entries` (§10).

### FR-9 — `resolve_gap_timeout` operator API *(should)*

New public method on `PgEventBus` completing the operator workflow (the DLQ has no
equivalent method today — resolution columns are updated via SQL — but the gap table
is new, so we can ship the API from day one):

```rust
/// Marks a gap-timeout record as resolved.
///
/// Use after confirming the skipped sequence came from a rolled-back
/// transaction (no action needed) or after replaying the late-committed
/// event through other means.
///
/// Returns `true` if a record was updated, `false` if no matching
/// unresolved record was found.
pub async fn resolve_gap_timeout(
    &self,
    id: Uuid,
    resolved_by: &str,
    resolution_notes: Option<&str>,
) -> Result<bool, SqlxError>
```

Sets `resolved_at = NOW()`, `resolved_by`, `resolution_notes` where `id` matches and
`resolved_at IS NULL`.

*Verify:* integration test `test_resolve_gap_timeout_marks_resolved` (§10).

### FR-10 — Public re-exports

Add `GapTimeoutCallback`, `GapTimeoutInfo`, and `GapTimeoutEntry` to:

1. the `pub use config::{...}` / type exports in `event_bus/mod.rs`, and
2. the `pub use event_bus::{...}` list in `epoch_pg/src/lib.rs`.

The `epoch` facade crate re-exports `epoch_pg::*` and needs no change.

*Verify:* doc test / compile check `use epoch_pg::{GapTimeoutCallback, GapTimeoutInfo, GapTimeoutEntry};`.

### FR-11 — Rustdoc on all new public items

All new public types, traits, fields, and methods carry rustdoc explaining purpose,
when the callback fires, the fire-and-forget contract, and the operational meaning of
a gap-timeout record (i.e., "a late-committing event at this sequence was never
delivered to this subscriber").

*Verify:* `cargo doc -p epoch_pg --no-deps` builds without missing-doc warnings;
`cargo clippy --workspace -- -D warnings` passes (the crate denies missing docs on
public items via existing lint configuration, if enabled; otherwise review).

---

## 6. Non-Functional Requirements

| ID | Requirement | Verification |
|----|-------------|--------------|
| NFR-1 | Gap-timeout recording must **not** block or gate checkpoint advancement. The DB write and callback run in a detached task; failures are logged and swallowed. | Code review + `test_gap_timeout_inserts_record` shows checkpoint advanced regardless |
| NFR-2 | Backward compatibility: `on_gap_timeout` defaults to `None`; `ReliableDeliveryConfig::default()` and `..Default::default()` construction continue to work. The only source-breaking change is for code that constructs the config exhaustively (field-by-field), consistent with prior field additions (`events_table`, `dispatch_mode`). | `cargo test --workspace` + updated `reliable_delivery_config_can_be_customized` |
| NFR-3 | `advance_contiguous_checkpoint` remains a pure synchronous function with no DB or logging side-effect dependencies. All async side-effects live in `mod.rs`. | Code review; function signature contains no pool/async |
| NFR-4 | All new code passes `cargo clippy --workspace -- -D warnings` and `cargo fmt --check`. | CI / local run |
| NFR-5 | Integration tests use fresh `Uuid::new_v4()` stream IDs, `#[serial]` + `#[tokio::test]`, and are idempotent against pre-existing state (e.g., scope assertions to the test's own subscriber ID), consistent with the existing harness. | Test review; repeated `cargo test` runs pass |
| NFR-6 | The insert is idempotent: re-recording the same `(bus_name, subscriber_id, skipped_sequence)` is a no-op (`ON CONFLICT DO NOTHING`). | Unit/integration test issuing the insert twice |

---

## 7. Acceptance Criteria (mapped to CLOUD-169)

| ID | Criterion | Maps to |
|----|-----------|---------|
| AC-1 | Every gap-timeout skip emits a `log::warn!` including bus name, subscriber ID, skipped sequence, and elapsed duration (no longer `info!`, no longer context-free) | Issue "must" #1 (log) — FR-1, FR-2 |
| AC-2 | A row exists in `epoch_event_bus_gap_timeouts` for each timed-out gap, queryable by bus/subscriber/sequence/unresolved | Issue "must" #1 (queryable record) — FR-6, FR-7, FR-8 |
| AC-3 | `on_gap_timeout` fires with a correct `GapTimeoutInfo` after persistence, enabling counters/alerts | Issue "must" #1 (counter) — FR-3, FR-4, FR-5, FR-7 |
| AC-4 | Test exists demonstrating: an event whose commit is delayed beyond `gap_timeout` is **visibly reported as skipped** (durable record + WARN), satisfying the issue's "delivered or visibly reported" acceptance via the reporting branch | Issue acceptance test — §10 `test_event_committed_after_gap_timeout_is_reported_as_skipped` |
| AC-5 | Snapshot fencing is evaluated and documented with a concrete go/no-go rationale and follow-up trigger | Issue "should" #2 — §8 |
| AC-6 | No existing tests break; in-order delivery paths produce zero gap-timeout records | Regression — §10 `test_no_gap_timeout_record_on_in_order_events` |

---

## 8. Snapshot Fencing Investigation (Should — evaluated, implementation deferred)

### 8.1 What it is

The Marten "high-water-mark" pattern: before advancing past a gap, check whether the
missing sequence could belong to a transaction that is **still open**. If any
transaction older than the gap is in-flight, hold the checkpoint instead of firing the
timeout. The timeout then becomes a fallback for genuinely rolled-back transactions
rather than the primary mechanism — eliminating the silent-skip mode entirely.

PostgreSQL exposes the needed primitives:

- `pg_current_snapshot()` (PG 13+) — `(xmin, xmax, xip_list)` of in-flight
  transaction IDs visible to the session
- `pg_snapshot_xmin(pg_current_snapshot())` — the oldest in-flight xid; any event
  insert by a transaction `>= xmin` may still commit
- `txid_current_snapshot()` / `txid_snapshot_xmin()` — pre-PG-13 equivalents

### 8.2 What an implementation would require

1. **New `txid BIGINT` column on `epoch_events`** populated at insert time via
   `pg_current_xact_id()::text::bigint` (PG 13+) or `txid_current()` — a migration on
   the hot events table touching every insert path (event store, custom
   `events_table` routing, triggers).
2. **Reader-side fencing**: track the bus's high-water mark as
   `max(global_sequence)` among events whose `txid` is **below** the snapshot's
   `xmin`; never advance a checkpoint past it. A gap below the high-water mark is
   provably rolled back and can be skipped immediately; a gap above it must be
   waited on.
3. **Fallback**: `gap_timeout` remains as a backstop for pathological cases (e.g.,
   prepared transactions held open indefinitely), but firing it would then be
   genuinely exceptional — and would still be recorded via this spec's machinery.

### 8.3 Decision: defer, with trigger condition

- Requires a schema migration on the hot write path before any production evidence
  justifies it; the PG-13 / pre-13 API split adds maintenance burden.
- The observability delivered by this spec is exactly the instrument needed to make
  the call: every gap-timeout now produces a durable record. If operators observe
  gap-timeout records whose sequence **later appears in `epoch_events`**
  (`SELECT g.* FROM epoch_event_bus_gap_timeouts g JOIN epoch_events e ON
  e.global_sequence = g.skipped_sequence` — committed-but-skipped events), that is
  direct evidence of the dangerous mode and triggers the fencing spec.
- **Follow-up:** open a dedicated issue ("snapshot fencing for PgEventBus gap
  resolution") referencing this section once CLOUD-169 ships; implement as its own
  spec if the above query ever returns rows in production, or proactively for
  deployments relying on long write transactions (spec 0007).

---

## 9. Files to Modify

| File | Change |
|------|--------|
| `epoch_pg/src/event_bus/subscriber_state.rs` | Add `SkippedGap`; change `advance_contiguous_checkpoint` return type to `Vec<SkippedGap>`; remove the gap-timeout `log::info!`; extend unit tests |
| `epoch_pg/src/event_bus/config.rs` | Add `GapTimeoutInfo`, `GapTimeoutCallback`, `on_gap_timeout` field; update `Default`, manual `Debug`, and the exhaustive-construction test; add unit tests |
| `epoch_pg/src/event_bus/mod.rs` | Consume `Vec<SkippedGap>` in `process_subscriber_for_batch`: WARN log, detached insert + callback (FR-7); add `GapTimeoutEntry`, `list_gap_timeouts`, `resolve_gap_timeout`; re-export new types |
| `epoch_pg/src/migrations/m009_create_gap_timeout_log.rs` | New migration (FR-6) |
| `epoch_pg/src/migrations/mod.rs` | Declare module, import, register m009 at end of `MIGRATIONS` |
| `epoch_pg/src/lib.rs` | Re-export `GapTimeoutCallback`, `GapTimeoutInfo`, `GapTimeoutEntry` |
| `epoch_pg/tests/pgeventbus_integration_tests.rs` | Integration tests (§10) |
| `CHANGELOG.md` | Entry under Unreleased: gap-timeout observability (WARN log, callback, `epoch_event_bus_gap_timeouts` table + m009, query/resolve APIs); note the exhaustive-construction source change |

No new dependencies are required (`async_trait`, `sqlx`, `tokio`, `chrono`, `uuid`,
`log` are all already in use).

---

## 10. Test Plan

### Unit tests — `subscriber_state.rs`

| Test | Assertion |
|------|-----------|
| `advance_contiguous_gap_timeout` (extend existing) | Returned `Vec<SkippedGap>` contains exactly the timed-out sequence(s); `gap_duration >= gap_timeout` |
| `advance_returns_empty_when_no_gap` (extend `advance_contiguous_no_gaps`) | In-order advancement returns an empty `Vec` |
| `advance_returns_empty_when_gap_not_timed_out` (extend `advance_contiguous_gap_not_yet_timed_out`) | Blocked-on-fresh-gap returns an empty `Vec` and checkpoint unchanged |
| `advance_contiguous_consecutive_gap_timeouts` (extend existing) | All consecutive timed-out gaps (6, 7, 8) appear in the returned `Vec`, in ascending order |
| `advance_gap_fill_returns_empty` (extend `advance_contiguous_gap_fills_clears_tracking`) | A gap that fills normally produces no `SkippedGap` |

### Unit tests — `config.rs`

| Test | Assertion |
|------|-----------|
| `on_gap_timeout_defaults_to_none` | `ReliableDeliveryConfig::default().on_gap_timeout.is_none()` |
| `gap_timeout_callback_can_be_set` | Field accepts an `Arc<dyn GapTimeoutCallback>`; `is_some()` |
| `gap_timeout_callback_can_be_invoked` | `#[tokio::test]` invoking the trait directly with a counting callback (mirrors `dlq_callback_can_be_invoked`) |
| `gap_timeout_info_fields` | Struct fields round-trip; `Clone` works |
| `config_debug_output_shows_callback_presence` (extend) | `Debug` renders `<callback>` / `None` for the new field |

### Integration tests — `pgeventbus_integration_tests.rs`

All `#[serial]` + `#[tokio::test]`, fresh `Uuid::new_v4()` stream IDs, short
`gap_timeout` (e.g., 500 ms) via `ReliableDeliveryConfig`, assertions scoped to the
test's own subscriber ID for idempotency.

| Test | What it checks | AC |
|------|----------------|----|
| `test_gap_timeout_inserts_record` | Create an artificial hole in `global_sequence` (raw SQL: bump the sequence with `setval`/explicit value, insert event at N+2 only); wait > `gap_timeout` + poll; assert a `epoch_event_bus_gap_timeouts` row exists for the missing sequence with this subscriber, **and** the subscriber's checkpoint advanced past it | AC-2 |
| `test_event_committed_after_gap_timeout_is_reported_as_skipped` | Same hole setup; after the timeout fires and the record exists, insert the "late" event at the missing sequence via raw SQL; assert the subscriber never receives it (observer's seen-set excludes it) **and** the gap-timeout record for that sequence is present — the "visibly reported as skipped" branch of the issue's acceptance test | AC-4 |
| `test_gap_timeout_callback_is_invoked` | Configure an atomic-counter `GapTimeoutCallback`; on gap timeout, assert it fires exactly once with matching `bus_name`, `subscriber_id`, `skipped_sequence`, and `gap_duration >= gap_timeout` | AC-3 |
| `test_list_gap_timeouts_returns_entries` | After a simulated timeout, `list_gap_timeouts(Some(sub), true, 0, 10)` returns the record; `unresolved_only` filtering works | AC-2 |
| `test_resolve_gap_timeout_marks_resolved` | `resolve_gap_timeout(id, "operator", Some("rolled back"))` returns `true`; record disappears from `unresolved_only` listing; second call returns `false` | FR-9 |
| `test_no_gap_timeout_record_on_in_order_events` | Publish in-order events normally; assert no gap-timeout rows exist for this subscriber | AC-6 |

### Regression

- Full `cargo test --workspace` green; `cargo clippy --workspace -- -D warnings`;
  `cargo fmt --check`.

---

## 11. Out of Scope

- Snapshot fencing implementation (§8 — follow-up issue after production data)
- Automatic replay of skipped events (operator action via `resolve_gap_timeout` /
  resolution columns is the intended workflow; replay tooling is a future concern)
- `gap_timeout` default change or tuning guidance beyond existing rustdoc (default
  stays 5 s)
- Retention/cleanup policy for `epoch_event_bus_gap_timeouts` (volume is expected to
  be near-zero in healthy systems; revisit if production data says otherwise)
- A built-in metrics dependency (counters are the consumer's job via the callback,
  matching the DLQ callback philosophy)
- Changes to `epoch_core`, `epoch_mem`, or `epoch_derive`
- Crate version bumps beyond a `CHANGELOG.md` entry
