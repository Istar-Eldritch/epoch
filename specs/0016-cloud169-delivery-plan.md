# Delivery Plan — CLOUD-169: Gap-Timeout Observability + Snapshot Fencing Investigation

**Spec:** [`specs/0016-gap-timeout-observability.md`](./0016-gap-timeout-observability.md)
**Issue:** Linear CLOUD-169
**Crate:** `epoch_pg`
**Commit scope:** `feat(pg)` (concept scope `event-bus`)
**Created:** 2026-06-10

---

## 1. Purpose

This document is the step-by-step, TDD-driven implementation plan for spec 0016. It
sequences the work so each change is independently compilable and verifiable, follows
the red → green → refactor cycle, and maps every step back to a Functional/Non-Functional
Requirement and Acceptance Criterion in the spec.

**Workflow gate:** per `CLAUDE.md §4.0`, no source changes begin until this plan is
approved by the developer.

---

## 2. Guiding Principles

- **TDD throughout.** Each work unit writes a failing test first, then the minimal
  code to pass, then refactors.
- **Pure core, side-effects at the edge.** `advance_contiguous_checkpoint` stays pure
  and synchronous (NFR-3); all logging, DB writes, and callbacks live in `mod.rs`.
- **Additive & non-blocking.** Observability never gates checkpoint advancement
  (NFR-1); recording is fire-and-forget.
- **Mirror existing patterns.** The DLQ callback (`DlqCallback`/`DlqInsertionInfo`),
  DLQ resolution columns (m006), and DLQ query APIs are the templates for the
  gap-timeout equivalents.
- **Bottom-up build order.** Pure types → config → migration → call-site wiring →
  query/resolve APIs → re-exports → integration tests, so the crate compiles at every
  commit boundary.

---

## 3. Pre-Flight

| Step | Action | Expected |
|------|--------|----------|
| P-1 | `cargo build -p epoch_pg` | Clean baseline |
| P-2 | `cargo test -p epoch_pg` (with a Postgres available for integration tests) | Green baseline; note current pass count |
| P-3 | `cargo clippy --workspace -- -D warnings` | Clean baseline |
| P-4 | Confirm a Postgres instance / `DATABASE_URL` for integration tests | Reachable |

Record the baseline test count so the final regression run can confirm no losses.

---

## 4. Work Breakdown (TDD order)

Each work unit (WU) lists: requirement mapping, the failing test to add first, the
implementation, and the verification command. Commits are suggested at WU boundaries.

### WU-1 — `SkippedGap` + pure return value (FR-1, NFR-3)

**File:** `epoch_pg/src/event_bus/subscriber_state.rs`

1. **RED.** Update/extend the existing unit tests to the new contract (they will not
   compile / will fail against the current `()` return):
   - `advance_contiguous_gap_timeout` → assert returned `Vec<SkippedGap>` contains
     exactly the timed-out sequence; `gap_duration >= gap_timeout`.
   - `advance_contiguous_no_gaps` → returns empty `Vec`.
   - `advance_contiguous_gap_not_yet_timed_out` → returns empty `Vec`, checkpoint
     unchanged.
   - `advance_contiguous_consecutive_gap_timeouts` → returns sequences (6, 7, 8) in
     ascending order.
   - `advance_contiguous_gap_fills_clears_tracking` → returns empty `Vec`.
2. **GREEN.**
   - Add the `pub(crate) struct SkippedGap { skipped_sequence: u64, gap_duration: Duration }`
     deriving `Debug, Clone, PartialEq, Eq`.
   - Change `advance_contiguous_checkpoint` to return `Vec<SkippedGap>`.
   - In the timeout branch: compute `gap_duration = first_seen.elapsed()`, push a
     `SkippedGap`, **remove the `log::info!`** (optionally replace with `log::debug!`).
   - Accumulate into a local `Vec` and return it; non-timeout/empty paths return
     `Vec::new()`.
3. **VERIFY.** `cargo test -p epoch_pg subscriber_state` (call sites in `mod.rs` will
   not yet consume the return value — that is WU-4; the function compiles standalone).

> Note: `mod.rs` currently calls this function for its side-effect. After WU-1 the
> crate may emit an `unused_must_use`/unused-value path; ignore the return at the call
> site temporarily (`let _ =`) only if needed to keep the crate compiling between
> WU-1 and WU-4, then wire it properly in WU-4. Prefer doing WU-1 and WU-4 in the same
> branch so the temporary discard is short-lived.

---

### WU-2 — `GapTimeoutInfo`, `GapTimeoutCallback`, `on_gap_timeout` field (FR-3, FR-4, FR-5, NFR-2)

**File:** `epoch_pg/src/event_bus/config.rs`

1. **RED.** Add unit tests (mirroring the DLQ tests):
   - `on_gap_timeout_defaults_to_none`.
   - `gap_timeout_callback_can_be_set` (assign `Arc<dyn GapTimeoutCallback>`, assert
     `is_some()`).
   - `gap_timeout_callback_can_be_invoked` (`#[tokio::test]`, counting callback,
     mirrors `dlq_callback_can_be_invoked`).
   - `gap_timeout_info_fields` (field round-trip + `Clone`).
   - Extend `config_debug_output_shows_callback_presence` to assert the new field
     renders `<callback>` / `None`.
2. **GREEN.**
   - Add `GapTimeoutInfo { bus_name, subscriber_id, skipped_sequence, gap_duration }`
     (`#[derive(Debug, Clone)]`).
   - Add `#[async_trait] pub trait GapTimeoutCallback: Send + Sync { async fn on_gap_timeout(&self, info: GapTimeoutInfo); }`.
   - Add `pub on_gap_timeout: Option<Arc<dyn GapTimeoutCallback>>` to
     `ReliableDeliveryConfig`; set to `None` in `Default`.
   - Extend the manual `Debug` impl to render the new field identically to
     `on_dlq_insertion`.
   - **Update `reliable_delivery_config_can_be_customized`** (exhaustive construction)
     to include the new field (NFR-2 source-break note).
3. **VERIFY.** `cargo test -p epoch_pg config`.

---

### WU-3 — Migration m009 (FR-6)

**Files:** `epoch_pg/src/migrations/m009_create_gap_timeout_log.rs` (new),
`epoch_pg/src/migrations/mod.rs`.

1. **RED.** Rely on existing migration meta-tests (`migrations_are_in_order`,
   `all_migrations_have_unique_versions`, `all_migrations_have_unique_names`); after
   registering m009 they assert version `9` slots in correctly. Optionally add a focused
   assertion that `MIGRATIONS.last()` has version 9 / name `create_gap_timeout_log`.
2. **GREEN.**
   - Create `m009_create_gap_timeout_log.rs` implementing `Migration` with
     `version() == 9`, `name() == "create_gap_timeout_log"`, executing the DDL from
     spec §FR-6 (table `epoch_event_bus_gap_timeouts`, `UNIQUE (bus_name,
     subscriber_id, skipped_sequence)`, sequence index, partial unresolved index).
   - In `mod.rs`: declare `mod m009_create_gap_timeout_log;`, import
     `CreateGapTimeoutLog`, append to the **end** of `MIGRATIONS`.
   - Follow the m006 structure for the resolution columns and the m008 file as the
     most recent template.
3. **VERIFY.** `cargo test -p epoch_pg migrations`.

---

### WU-4 — Call-site wiring: WARN log + detached insert + callback (FR-2, FR-7, NFR-1)

**File:** `epoch_pg/src/event_bus/mod.rs` (`process_subscriber_for_batch`).

1. **RED.** Integration test `test_gap_timeout_inserts_record` (see §5) — fails until
   the insert path exists. (Integration tests are written in WU-7 but this WU is what
   makes them pass; if developing strictly red-first, stub the test here and implement.)
2. **GREEN.** After `advance_contiguous_checkpoint` returns its `Vec<SkippedGap>`:
   - For each `SkippedGap`:
     - Emit `log::warn!` with `bus_name = config.events_table`, `subscriber_id`,
       `skipped_sequence`, `gap_duration` (FR-2 message shape).
     - Clone the data needed (`checkpoint_pool`, `config.events_table`,
       `subscriber_id`, `config.on_gap_timeout.clone()`, sequence, duration) and
       `tokio::spawn` a detached task that:
       1. Runs the `INSERT ... ON CONFLICT (bus_name, subscriber_id,
          skipped_sequence) DO NOTHING` binding `gap_duration.as_millis() as i64`.
       2. On error → `log::error!` and return.
       3. On success → if `on_gap_timeout` is `Some(cb)`, build `GapTimeoutInfo` and
          `cb.on_gap_timeout(info).await`.
   - **Do not await** the spawned task; checkpoint advance/flush is unchanged (NFR-1).
3. **VERIFY.** `cargo build -p epoch_pg`; integration `test_gap_timeout_inserts_record`
   passes (after WU-7 harness exists).

---

### WU-5 — Query API: `GapTimeoutEntry` + `list_gap_timeouts` (FR-8)

**File:** `epoch_pg/src/event_bus/mod.rs`.

1. **RED.** Integration test `test_list_gap_timeouts_returns_entries` (§5).
2. **GREEN.**
   - Add `pub struct GapTimeoutEntry { ... }` with all columns (spec FR-8), `Debug, Clone`.
   - Add `pub async fn list_gap_timeouts(&self, subscriber_id: Option<&str>,
     unresolved_only: bool, offset: u64, limit: u64) -> Result<Vec<GapTimeoutEntry>, SqlxError>`:
     scoped to `bus_name = config.events_table`, optional subscriber filter, optional
     `resolved_at IS NULL` filter, ordered by `timed_out_at ASC`, paginated. Mirror the
     existing `get_dlq_entries_paginated` style. Map `skipped_sequence` to `u64`,
     `gap_duration_ms` to `i64`.
3. **VERIFY.** `cargo test -p epoch_pg` + the integration test.

---

### WU-6 — Resolve API: `resolve_gap_timeout` (FR-9 — should)

**File:** `epoch_pg/src/event_bus/mod.rs`.

1. **RED.** Integration test `test_resolve_gap_timeout_marks_resolved` (§5).
2. **GREEN.** Add `pub async fn resolve_gap_timeout(&self, id: Uuid, resolved_by: &str,
   resolution_notes: Option<&str>) -> Result<bool, SqlxError>` issuing
   `UPDATE ... SET resolved_at = NOW(), resolved_by = $.., resolution_notes = $..
   WHERE id = $.. AND resolved_at IS NULL`; return `rows_affected() > 0`.
3. **VERIFY.** integration test passes; second call on same id returns `false`.

---

### WU-7 — Public re-exports (FR-10)

**Files:** `epoch_pg/src/event_bus/mod.rs`, `epoch_pg/src/lib.rs`.

1. Add `GapTimeoutCallback`, `GapTimeoutInfo`, `GapTimeoutEntry` to the event-bus
   `pub use` lists and to `epoch_pg/src/lib.rs`'s `pub use event_bus::{...}`.
2. **VERIFY.** Add a compile/doc check: `use epoch_pg::{GapTimeoutCallback,
   GapTimeoutInfo, GapTimeoutEntry};`. The `epoch` facade needs no change (re-exports
   `epoch_pg::*`); confirm with `cargo build -p epoch`.

---

### WU-8 — Integration tests (Test Plan §10) (AC-2, AC-3, AC-4, AC-6, FR-9, NFR-5, NFR-6)

**File:** `epoch_pg/tests/pgeventbus_integration_tests.rs`.

All `#[serial]` + `#[tokio::test]`, fresh `Uuid::new_v4()` stream IDs, short
`gap_timeout` (~500 ms), assertions scoped to the test's own subscriber ID (NFR-5).

| Test | Maps to |
|------|---------|
| `test_gap_timeout_inserts_record` — artificial `global_sequence` hole; wait > timeout + poll; assert row exists and checkpoint advanced past it | AC-2, NFR-1 |
| `test_event_committed_after_gap_timeout_is_reported_as_skipped` — insert "late" event at the skipped seq after timeout; assert subscriber never sees it **and** the record exists | AC-4 |
| `test_gap_timeout_callback_is_invoked` — atomic-counter callback fires once with matching fields; `gap_duration >= gap_timeout` | AC-3 |
| `test_list_gap_timeouts_returns_entries` — listing + `unresolved_only` filter | AC-2 |
| `test_resolve_gap_timeout_marks_resolved` — resolve returns `true`, drops from unresolved listing, second call `false` | FR-9 |
| `test_no_gap_timeout_record_on_in_order_events` — in-order publish produces zero rows | AC-6 |
| Idempotency check (insert twice → one row) | NFR-6 |

**Test helper to add:** a SQL utility that creates a deterministic hole in
`global_sequence` (e.g., `setval` / explicit insert at `N+2`) scoped to a fresh stream,
reused by the hole-based tests.

---

### WU-9 — Docs, changelog, snapshot-fencing write-up (FR-11, AC-5)

1. **Rustdoc (FR-11).** Ensure every new public item documents purpose, when the
   callback fires, the fire-and-forget contract, and the operational meaning of a
   gap-timeout record. `cargo doc -p epoch_pg --no-deps` clean.
2. **Snapshot fencing (AC-5).** The investigation and go/no-go rationale already live
   in spec §8 (decision: **defer**, with the committed-but-skipped JOIN query as the
   trigger condition). No code; confirm §8 is referenced from the follow-up. Action
   item: after merge, open a follow-up issue "snapshot fencing for PgEventBus gap
   resolution" referencing §8.
3. **CHANGELOG.md.** Add an Unreleased entry: gap-timeout observability (WARN log,
   `on_gap_timeout` callback, `epoch_event_bus_gap_timeouts` table + m009, `list_gap_timeouts`
   / `resolve_gap_timeout` APIs); note the exhaustive-construction source-compat change.

---

## 5. Verification Matrix

| Spec item | Verified by |
|-----------|-------------|
| FR-1 / NFR-3 | `subscriber_state.rs` unit tests (WU-1) |
| FR-2 | Code review + integration record presence (WU-4/WU-8) |
| FR-3 / FR-4 / FR-5 / NFR-2 | `config.rs` unit tests (WU-2) |
| FR-6 | Migration meta-tests + `setup()` applies m009 (WU-3) |
| FR-7 / NFR-1 | `test_gap_timeout_inserts_record`, `test_gap_timeout_callback_is_invoked` (WU-8) |
| FR-8 | `test_list_gap_timeouts_returns_entries` (WU-8) |
| FR-9 | `test_resolve_gap_timeout_marks_resolved` (WU-8) |
| FR-10 | `use epoch_pg::{...}` compile check (WU-7) |
| FR-11 | `cargo doc -p epoch_pg --no-deps` (WU-9) |
| NFR-4 | `cargo clippy --workspace -- -D warnings`, `cargo fmt --check` (§6) |
| NFR-5 | Test review + repeated `cargo test` runs (WU-8) |
| NFR-6 | Idempotency test (WU-8) |
| AC-1..AC-6 | Mapped per WU above; AC-5 via spec §8 (WU-9) |

---

## 6. Final Regression Gate

Run before declaring done (CLAUDE.md §5.0, NFR-4):

```bash
cargo fmt --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
cargo doc -p epoch_pg --no-deps
```

All green, no missing-doc warnings, integration tests pass against Postgres, and the
baseline test count from P-2 is preserved or increased.

---

## 7. Suggested Commit Sequence

| # | Commit | WUs |
|---|--------|-----|
| 1 | `feat(pg): return SkippedGap from gap-timeout advancement` | WU-1 |
| 2 | `feat(pg): add GapTimeoutCallback and on_gap_timeout config` | WU-2 |
| 3 | `feat(pg): add m009 gap-timeout log table` | WU-3 |
| 4 | `feat(pg): record gap timeouts with WARN log, durable insert, callback` | WU-4 |
| 5 | `feat(pg): add list_gap_timeouts and resolve_gap_timeout APIs` | WU-5, WU-6 |
| 6 | `feat(pg): re-export gap-timeout public types` | WU-7 |
| 7 | `test(pg): integration tests for gap-timeout observability` | WU-8 |
| 8 | `docs(pg): changelog + rustdoc for gap-timeout observability` | WU-9 |

(WU-1 and WU-4 may be developed together to avoid a temporary return-value discard; see
the note in WU-1.)

---

## 8. Risks & Mitigations

| Risk | Mitigation |
|------|------------|
| Temporary unused return value between WU-1 and WU-4 | Develop WU-1 + WU-4 on one branch; use `let _ =` only transiently |
| Integration tests flaky due to timing (`gap_timeout` race) | Use short timeout + polling with bounded retries; scope assertions to own subscriber ID |
| Exhaustive-construction test breaks downstream | Documented source-compat note (NFR-2); CHANGELOG entry |
| Detached task outliving pool on shutdown | Insert errors are logged & swallowed (NFR-1); no panic; acceptable for fire-and-forget |
| Migration ordering conflict if another m009 lands | Confirm m008 is last at branch time; rebase + bump if a newer migration appears |

---

## 9. Out of Scope (per spec §11)

Snapshot-fencing implementation, automatic replay, `gap_timeout` default tuning,
retention/cleanup policy, built-in metrics dependency, and changes to
`epoch_core` / `epoch_mem` / `epoch_derive`. No crate version bumps beyond CHANGELOG.
