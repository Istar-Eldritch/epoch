# Spec 0016: Gap-Timeout Observability + Snapshot Fencing Investigation

**Issue:** Linear CLOUD-169
**Status:** Draft
**Created:** 2026-06-10

---

## 1. Problem Statement

`PgEventBus` uses a gap-resolution heuristic (`gap_timeout`, default 5 s) to handle
gaps in `global_sequence`. When a gap is older than `gap_timeout`, the system assumes
the inserting transaction was rolled back and advances the checkpoint past the missing
sequence. If the transaction was actually still in-flight (slow batch import, lock
contention, long `AggregateTransaction` with many `handle()` calls — made more likely
by spec 0007 transaction support), the committed event is **permanently skipped** with:

- Only an `INFO`-level log line (easy to miss)
- No durable record of what was skipped
- No counter or callback to alert operators
- No way for operators to detect or replay the skipped range after the fact

This is a silent data-loss mode in the reliability layer.

---

## 2. Scope

| Priority | Work |
|----------|------|
| **Must** | WARN log + structured `on_gap_timeout` callback + durable `epoch_event_bus_gap_timeouts` table (migration m009) |
| **Should** | Investigate and document snapshot fencing (`pg_current_snapshot()`) as a future mitigation; determine whether adding a `txid` column to `epoch_events` is warranted |
| **Deferred** | Full snapshot fencing implementation (separate spec/issue once observability data is available) |

---

## 3. Codebase Grounding

| Fact | Location | Implication |
|------|----------|-------------|
| Gap timeout fires at `log::info!("Advancing past gap at seq {}", next, gap_timeout)` | `subscriber_state.rs:103` | Change to `log::warn!`; add subscriber context |
| `advance_contiguous_checkpoint` returns `()` | `subscriber_state.rs:76` | Change to `Vec<SkippedGap>` to bubble timed-out sequences up to callers without adding async to the pure function |
| Called once per batch per subscriber | `mod.rs:142` | Returned `Vec<SkippedGap>` processed in the `process_subscriber_for_batch` async context, where DB writes and callbacks are already done |
| `on_dlq_insertion: Option<Arc<dyn DlqCallback>>` with `DlqInsertionInfo` struct | `config.rs:20–51` | Exact pattern to mirror for `on_gap_timeout` |
| DLQ write is fire-and-forget (errors logged, doesn't block checkpoint) | `mod.rs` / `retry.rs` | Same contract for gap-timeout DB write |
| Next migration slot is m009 | `migrations/mod.rs:MIGRATIONS` | New file `m009_create_gap_timeout_log.rs` |
| `epoch_event_bus_dlq` has `resolved_at`, `resolved_by`, `resolution_notes` columns | `m006_add_dlq_resolution_columns.rs` | Mirror these on `epoch_event_bus_gap_timeouts` for operator workflow |
| `gap_first_seen: HashMap<u64, Instant>` uses `tokio::time::Instant` | `subscriber_state.rs:39` | `Instant` cannot be converted to wall-clock time; store `gap_duration` in the `SkippedGap` (computed at timeout via `first_seen.elapsed()`); derive `gap_observed_at ≈ NOW() - gap_duration_ms` at DB-write time |
| `PgEventBus` already exposes `list_dlq_entries` / `get_dlq_entries` query APIs | `mod.rs` | Mirror with `list_gap_timeouts` |

---

## 4. Functional Requirements

### FR-1 — WARN-level log

When a gap is timed out in `advance_contiguous_checkpoint`, emit a `log::warn!` (was
`log::info!`) that includes: the skipped sequence, the elapsed duration, and the
subscriber ID. The subscriber ID is not available in `subscriber_state.rs`; it is
provided by the caller.

### FR-2 — `SkippedGap` return value

Change `advance_contiguous_checkpoint` from `() ` to `Vec<SkippedGap>` where:

```rust
/// Describes a single gap that was advanced past due to timeout.
/// Returned by `advance_contiguous_checkpoint` for each sequence the caller skipped.
pub(crate) struct SkippedGap {
    /// The `global_sequence` that was skipped.
    pub skipped_sequence: u64,
    /// How long this gap was observed before the timeout fired.
    pub gap_duration: Duration,
}
```

The caller (`mod.rs`) is responsible for the WARN log, DB write, and callback using
the returned `Vec<SkippedGap>`.

### FR-3 — `GapTimeoutInfo` struct

New public struct in `config.rs`, mirroring `DlqInsertionInfo`:

```rust
/// Information about a gap that was advanced past due to timeout.
/// Passed to the [`GapTimeoutCallback`].
#[derive(Debug, Clone)]
pub struct GapTimeoutInfo {
    /// The name of the bus on which the gap occurred.
    pub bus_name: String,
    /// The subscriber that observed the gap.
    pub subscriber_id: String,
    /// The `global_sequence` that was skipped.
    pub skipped_sequence: u64,
    /// How long the gap was observed before the timeout fired.
    pub gap_duration: Duration,
}
```

### FR-4 — `GapTimeoutCallback` trait

New public async trait in `config.rs`, mirroring `DlqCallback`:

```rust
/// Callback invoked when a gap is advanced past due to timeout.
///
/// Implementations should be lightweight. Errors from the callback are logged
/// but do **not** affect checkpoint advancement — the gap is always skipped
/// regardless of callback outcome.
#[async_trait]
pub trait GapTimeoutCallback: Send + Sync {
    async fn on_gap_timeout(&self, info: GapTimeoutInfo);
}
```

### FR-5 — `on_gap_timeout` field on `ReliableDeliveryConfig`

Add to `ReliableDeliveryConfig`:

```rust
/// Optional callback invoked when a gap is advanced past due to timeout.
///
/// Fires after the gap timeout record has been persisted to
/// `epoch_event_bus_gap_timeouts`. Errors from the callback are logged
/// but do not affect checkpoint advancement.
///
/// Default: `None` (no callback)
pub on_gap_timeout: Option<Arc<dyn GapTimeoutCallback>>,
```

Default: `None`. Include in `Debug` impl as `Some(<callback>)` / `None` (mirroring
the DLQ field).

### FR-6 — Migration m009: `epoch_event_bus_gap_timeouts` table

New file `epoch_pg/src/migrations/m009_create_gap_timeout_log.rs`:

```sql
CREATE TABLE IF NOT EXISTS epoch_event_bus_gap_timeouts (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    bus_name        VARCHAR(255)  NOT NULL,
    subscriber_id   VARCHAR(255)  NOT NULL,
    skipped_sequence BIGINT       NOT NULL,
    gap_duration_ms  BIGINT       NOT NULL,
    timed_out_at     TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    resolved_at      TIMESTAMPTZ,
    resolved_by      VARCHAR(255),
    resolution_notes TEXT
);

CREATE INDEX IF NOT EXISTS idx_epoch_gap_timeouts_bus_subscriber
    ON epoch_event_bus_gap_timeouts (bus_name, subscriber_id);

CREATE INDEX IF NOT EXISTS idx_epoch_gap_timeouts_sequence
    ON epoch_event_bus_gap_timeouts (skipped_sequence);

CREATE INDEX IF NOT EXISTS idx_epoch_gap_timeouts_unresolved
    ON epoch_event_bus_gap_timeouts (resolved_at)
    WHERE resolved_at IS NULL;
```

The `resolved_at` / `resolved_by` / `resolution_notes` columns mirror the DLQ pattern,
giving operators an in-database workflow for recording whether the skipped event was
confirmed rolled back or was replayed via another mechanism.

### FR-7 — Durable persistence on gap timeout

In `mod.rs`, after `advance_contiguous_checkpoint` returns, for each `SkippedGap`:

1. Emit `log::warn!` with bus name, subscriber ID, skipped sequence, and duration.
2. Spawn a fire-and-forget `tokio::task` (or use `tokio::spawn`) to insert a row into
   `epoch_event_bus_gap_timeouts`. Errors are logged but must not panic or block the
   main event loop.
3. If `config.on_gap_timeout` is `Some(cb)`, invoke `cb.on_gap_timeout(info).await`
   **after** the DB write attempt (errors logged, no effect on checkpoint advancement).

The pool used for the gap timeout write is the same `checkpoint_pool` already available
in the batch loop scope.

### FR-8 — `list_gap_timeouts` query API

Add a public method to `PgEventBus` analogous to `list_dlq_entries`:

```rust
pub async fn list_gap_timeouts(
    &self,
    subscriber_id: Option<&str>,
    unresolved_only: bool,
    limit: u32,
    offset: u32,
) -> Result<Vec<GapTimeoutEntry>, PgEventBusError>
```

Where `GapTimeoutEntry` is a new public struct with all columns of
`epoch_event_bus_gap_timeouts`.

### FR-9 — Rustdoc on all new public items

All new public types, traits, and methods must have rustdoc comments explaining
purpose, when the callback fires, and the fire-and-forget contract.

---

## 5. Non-Functional Requirements

| ID | Requirement |
|----|-------------|
| NFR-1 | Gap timeout recording must **not** block checkpoint advancement. DB write and callback are detached from the main processing loop. |
| NFR-2 | Backward compatibility: `on_gap_timeout` defaults to `None`; existing `ReliableDeliveryConfig` construction without the new field must still compile (add to `Default` impl and to existing builder tests). |
| NFR-3 | `advance_contiguous_checkpoint` remains a pure synchronous function with no DB dependencies. All async side-effects live in `mod.rs`. |
| NFR-4 | All new code passes `cargo clippy --workspace -- -D warnings` and `cargo fmt --check`. |
| NFR-5 | Integration tests use fresh `Uuid::new_v4()` stream IDs and are `#[serial]` + `#[tokio::test]` consistent with the existing `pgeventbus_integration_tests.rs` harness. |

---

## 6. Success Criteria

| ID | Criterion |
|----|-----------|
| SC-1 | `log::warn!` is emitted (not `info!`) every time a gap is advanced past, including bus name, subscriber ID, skipped sequence, and elapsed duration |
| SC-2 | A row is inserted into `epoch_event_bus_gap_timeouts` for each timed-out gap |
| SC-3 | `on_gap_timeout` callback is invoked with correct `GapTimeoutInfo` |
| SC-4 | `list_gap_timeouts()` returns persisted records |
| SC-5 | No existing tests are broken; gap-unaffected paths are regression-free |
| SC-6 | Snapshot fencing is documented as a follow-up issue (see §7) |

---

## 7. Snapshot Fencing Investigation (Deferred)

### 7.1 What it is

The Marten high-water-mark pattern: before advancing past a gap, check whether the
missing sequence was produced by a transaction that is **still open**. If so, hold the
checkpoint and wait rather than firing the timeout. The timeout becomes a fallback for
genuinely rolled-back transactions, not the primary mechanism.

PostgreSQL exposes this via:
- `pg_current_snapshot()` (Postgres 13+) — returns `(xmin, xmax, xip_list)` for the
  current session's snapshot
- `txid_current_snapshot()` (older, deprecated in PG 13+) — equivalent

### 7.2 What would be required to implement it

1. **New column `txid BIGINT` on `epoch_events`** — populated at insert time via
   `txid_current()::bigint` (or `pg_current_xact_id()::bigint` on PG 13+). This is
   a new migration on the hot events table and affects every event insert.
2. **Lookup at gap-check time** — query
   `SELECT txid FROM epoch_events WHERE global_sequence = $1` for the missing sequence
   (it may not exist yet, which is the normal "still in-flight" case). When the
   sequence does exist but the `txid` is still in `xip_list` of
   `pg_current_snapshot()`, hold the checkpoint.
3. **Fallback** — `gap_timeout` remains as the backstop for transactions that abort
   without inserting the event.

### 7.3 Decision

Deferred. Reasons:

- Requires a schema migration on `epoch_events` (hot write path) before any
  observability data is available to justify it
- The Pg 13+ / older API divergence adds maintenance burden
- The observability work in this spec (SC-1 through SC-4) will surface whether
  real-world gap timeouts are from rolled-back transactions (expected/safe) or
  from slow commits (dangerous). That data should drive the fencing decision
- The `txid` column and fencing logic should be a standalone spec/issue once
  production data is available

**Action:** Track as a follow-up after CLOUD-169 is live in production.

---

## 8. Files to Modify

| File | Change |
|------|--------|
| `epoch_pg/src/event_bus/subscriber_state.rs` | Change return type of `advance_contiguous_checkpoint` from `()` to `Vec<SkippedGap>`; add `SkippedGap` struct; emit the gaps rather than logging; remove `log::info!` from this module for gap timeouts |
| `epoch_pg/src/event_bus/config.rs` | Add `GapTimeoutInfo`, `GapTimeoutCallback`, `on_gap_timeout` field on `ReliableDeliveryConfig`, `Debug` impl update |
| `epoch_pg/src/event_bus/mod.rs` | Use returned `Vec<SkippedGap>` after `advance_contiguous_checkpoint`; WARN log; fire-and-forget DB write; callback invocation; add `list_gap_timeouts` + `GapTimeoutEntry` |
| `epoch_pg/src/migrations/m009_create_gap_timeout_log.rs` | New migration |
| `epoch_pg/src/migrations/mod.rs` | Register m009 in `MIGRATIONS` array |
| `epoch_pg/tests/pgeventbus_integration_tests.rs` | Integration tests for gap timeout recording, callback, and query API |
| `epoch_pg/src/lib.rs` | Re-export `GapTimeoutInfo`, `GapTimeoutCallback`, `GapTimeoutEntry` |

---

## 9. Test Plan

### Unit tests (in `subscriber_state.rs`)

- Extend existing `advance_contiguous_gap_timeout` test to assert the returned
  `Vec<SkippedGap>` contains the correct sequence and that `gap_duration >= gap_timeout`.
- Assert non-timed-out gaps return an **empty** `Vec`.
- Assert multiple consecutive timed-out gaps all appear in the returned `Vec`.

### Unit tests (in `config.rs`)

- `on_gap_timeout_defaults_to_none`
- `gap_timeout_callback_can_be_set`
- `gap_timeout_info_fields` (struct field assertions)

### Integration tests (in `pgeventbus_integration_tests.rs`)

All use fresh `Uuid::new_v4()` stream IDs and the existing `setup()` harness.

| Test | What it checks |
|------|---------------|
| `test_gap_timeout_inserts_record` | Insert event at seq N+2 via raw SQL (bypassing bus); wait > `gap_timeout`; next timer tick should produce a `epoch_event_bus_gap_timeouts` row for seq N+1 |
| `test_gap_timeout_callback_is_invoked` | Configure an `AtomicBool`-backed `GapTimeoutCallback`; assert it fires on gap timeout |
| `test_list_gap_timeouts_returns_entries` | After a simulated gap timeout, `list_gap_timeouts(None, true, 10, 0)` returns at least one entry |
| `test_no_gap_timeout_record_on_in_order_events` | Normal in-order event delivery; assert `epoch_event_bus_gap_timeouts` is empty |

---

## 10. Out of Scope

- Snapshot fencing implementation (see §7)
- Automatic replay of skipped events (operator action via `resolved_at` column is the intended workflow)
- `gap_timeout` tuning guidance beyond existing rustdoc (no change to default of 5 s)
- Changes to `epoch_core`, `epoch_mem`, or `epoch_derive`
- Crate version bumps beyond a `CHANGELOG.md` entry
