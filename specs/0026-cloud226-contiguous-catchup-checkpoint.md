# Spec 0026: Contiguous Catch-Up Checkpoint

**Issue:** CLOUD-226 · **Status:** Implemented · **Crate:** `epoch_pg`
**Scope:** `fix(pg)` — no migration, schema, or public-API change.
**Supersedes:** spec 0024 OQ-4 · **Related:** CLOUD-227 (`ReplayAlways` HWM, out of scope).

> Anchors are symbol names, not line numbers.

## 1. Problem

The catch-up path persisted a checkpoint it had not earned. Because `global_sequence` is assigned by non-transactional `nextval()` (spec 0019), a page of committed rows can hold a gap that fills later (e.g. seq 4 in an open txn, seq 5 already committed).

`catch_up_from_checkpoint` advanced a linear `PendingCheckpoint` per row *seen* and flushed the max: given `1,2,3,5` it persisted `5`. The listener then seeded `SubscriberState::new(5)`, so event 4, committing moments later, was delivered to nobody, silently: no WARN, no gap-timeout row, no DLQ entry, and `wait_until_caught_up` returned `Ok(true)` over a read model missing an event.

**Two writers, not one.** `subscribe()` runs `catch_up_from_checkpoint` *and* a second buffer-drain pass that also flushed a linear max. Since `flush_checkpoint` is a blind, non-monotonic `DO UPDATE SET last_global_sequence = EXCLUDED...`, the drain could overwrite a conservative checkpoint within the same call. Fixing only catch-up left the bug reachable.

**Why now:** spec 0024's R2 pass reran this unfenced catch-up over every subscriber on every `start_listener()`, moving the blast radius from once-per-subscription to once-per-boot, precisely when in-flight transactions are most likely (rolling restart under write load).

## 2. Goals / Non-Goals

**Goals.** Neither catch-up writer may persist a checkpoint above a hole; termination preserved when a hole never fills.
**Non-Goals.** No change to the live path's gap machinery, public API, error types, or schema. Not fixing CLOUD-227/228/229/230.

## 3. Rejected: reuse the live-path fence

Giving catch-up a `SubscriberState` and calling `advance_contiguous_checkpoint` per page is unsound. That function's unstated precondition is that the visible-sequence set is *complete* from `contiguous_checkpoint` upward. The live path guarantees this by re-querying from `min_checkpoint` (below the gap) every batch. Catch-up cannot: its pagination cursor must advance past the hole (§4.1), so later pages query `> cursor` and structurally cannot re-observe the gap even after it commits. The resolver then sees a permanent hole and, once `xmin` passes the captured `fence_xmax`, reports `SkipReason::FenceCleared` — a *committed* event skipped permanently, more quietly than the original bug. The fence proves the writer **finished**, not **aborted**; that inference is only valid against a current view of the gap region.

Also: the `gap_timeout` backstop cannot fire inside a bounded pass (a gap is only recorded on first observation; verdicts need a later call that a pass ending at head never makes), and threading `SubscriberState` would allocate one `HashSet` entry per event above the hole on the boot path.

## 4. Design: contiguous-prefix counter

Catch-up need not resolve gaps; it just must stop lying about what it processed. The live loop is entered immediately after and already owns gap resolution.

- **4.1 Split cursor from checkpoint.** Pagination cursor keeps advancing by *max sequence seen* (else a persistent hole re-reads the same page forever). Persisted checkpoint becomes the highest contiguous prefix.
- **4.2 The counter.** Rows arrive ascending, so the prefix is an O(1) running counter: seed `contiguous` from the starting checkpoint; per row, if `seq == contiguous + 1` advance and remember its `event_id`, else stop advancing for the rest of the pass. Keep paginating to head. No `SubscriberState`, snapshot, fence, or backstop.
  - **Flushing follows the configured `CheckpointMode`**, publishing only `contiguous` and its `event_id`. An earlier draft of this spec said "flush once at pass end"; that was wrong. `Synchronous` is the default and its documented contract is that at most one event is redelivered after a crash, so an all-or-nothing pass would silently make a 10M-event backfill that died near the end redeliver all of it, and would ignore `Batched`'s `batch_size`/`max_delay_ms` outright. Incremental flushing is sound because `contiguous` is monotone within a pass, so a value published early can never sit above a hole. A final unconditional flush still runs at pass end so a `Batched` pass that never crosses a threshold cannot leave an advance unflushed. A hole that fills mid-pass is deliberately not detected: the live loop re-reads and delivers it.
- **4.3 Drain gets the same treatment.** `catch_up_from_checkpoint` returns the **pagination cursor** (correct `> current_sequence` lower bound, no full-backlog reprocess) and the **contiguous** value; the drain continues the same counter and flushes only contiguous. Because `flush_checkpoint` is non-monotonic, this is a requirement, not an optimisation.
- **4.4 At-least-once above a hole** is the existing restart contract; re-delivery is pre-existing accepted behaviour.
- **4.5 `ReplayAlways` unchanged, still wrong.** `advance_catchup_prefix` early-returns to the in-memory HWM and never writes the table, so the counter doesn't apply. Resuming from a surviving linear-max HWM means such a subscriber above a hole still misses the event on in-process restart. Tracked in CLOUD-227.

## 5. Requirements

- **R1.** `catch_up_from_checkpoint` MUST NOT persist a checkpoint above a hole.
- **R2.** The drain MUST NOT flush above the contiguous value carried from catch-up.
- **R3.** Both MUST terminate when a hole is held open for the whole pass.
- **R4.** A checkpoint's `last_event_id` MUST match its `last_global_sequence`.
- **R5.** Readiness MUST NOT report caught-up while a checkpoint is legitimately held below a hole. *(Observable change: a gate that returned `true` over a lost event now blocks until the hole resolves.)*
- **R6.** `ReplayAlways` unchanged. **R7.** No public API, error-type, or schema change.

## 6. Test Plan

Two constraints: (a) absolute sequence numbers are unusable — five parallel binaries share `epoch_events` and `#[serial]` only serialises within a binary, so every assertion is relative to sequences from `INSERT ... RETURNING global_sequence`; (b) the open-transaction pattern (`test_in_flight_transaction_gap_is_held`, with `start_fence_test_bus` / `insert_committed_event`) already exists — reuse it, don't replicate its sleep-based shape five times. Prefer unit tests where they suffice.

1. **Unit:** prefix counter stops at first hole; checkpoint below held seq, returned cursor above it. (R1, R3)
2. **Unit:** multi-page (`catch_up_batch_size` 2, hole on non-final page); counter survives boundaries, does not resume. (R1)
3. **Unit:** positive control, no hole — advances to head with matching `last_event_id`. (R1, R4)
4. **Integration:** end-to-end delivery of an event committed after catch-up; subscribe *before* `start_listener` so the R2 pass is exercised; bounded `wait_until_caught_up`. (R1, R5)
5. **Integration:** `subscribe()` drain does not overwrite — checkpoint still below hole after return. Catches the second writer. (R2)
6. **Unit:** `ReplayAlways` regression guard. (R6)

## 7. Failure Modes

- Hole held all pass → checkpoint below it, live loop re-delivers (= restart contract).
- Long txn pins hole → checkpoint stops, readiness blocks (correct, newly visible); the live path's `gap_timeout` still records it. Catch-up contributes no gap-timeout rows, by design.
- Cold read model with an early hole re-delivers a backlog on next boot, bounded by the hole's lifetime.
- `ReplayAlways` retains its own loss window (CLOUD-227).

## 8. Acceptance Criteria

1. New tests pass; existing gap-fence/gap-timeout tests pass **unchanged** (proves live path untouched).
2. Reverting the fix makes tests 1, 2, 5 fail — verified mechanically by stashing, not by inspection.
3. `cargo clippy --all-targets -p epoch_pg -p epoch_core -- -D warnings` and `cargo fmt --check` clean.
4. Event-bus binary passes in isolation and under a concurrent writer on `epoch_events`, on consecutive runs. (A green `--workspace` pair is a weak gate: shared DB, load-sensitive tests.)

## 9. Open Questions

- **OQ-1.** Report that catch-up stopped below a hole, so a gate can distinguish "caught up" from "caught up to a hole"? Deferred: additive, and R5 already makes the gate honest.
- **OQ-2.** Any consumer of the old linear-max return value? In-repo, only `subscribe()` (handled §4.3); out-of-repo impossible (`pub(crate)`).

## 10. Implementation Summary

Delivered across 5 phases in `epoch_pg`:

- **P1 — prefix counter** (`89813b50`): replaced the linear `PendingCheckpoint` in `catch_up_from_checkpoint` with a contiguous-prefix counter; return the pagination cursor; flush contiguous + its `event_id`. Saga test relaxed for at-least-once above a hole (`2b3f3475`). *(R1, R3, R4)*
- **P2 — drain** (`862d35e6`): consolidated prefix advancement into a shared function so `subscribe()`'s buffer drain continues the same counter and never flushes above contiguous. *(R2, R4)*
- **P3 — unit tests** (`62393d4b`, review fixes `3dcebb41`): hole-stop, multi-page, positive control, `ReplayAlways`. Held-txn swapped for `nextval` to avoid TRUNCATE lock contention (`07852cb7`). *(R1, R3, R4, R6)*
- **P4 — integration tests** (`df090e0b`, `71074e74`, `a6306b96`): end-to-end delayed delivery (subscribe before listener) and drain-no-overwrite; sequences captured via `RETURNING`, stable channel names. *(R1, R2, R5)*
- **P5 — hygiene/docs** (`9602a237`, `d93fa3b6`): CHANGELOG `### Fixed` entry noting the R5 readiness change; spec 0024 OQ-4 marked resolved; saga checkpoint-advance assertion relaxed for contiguous-prefix hold. *(R5, R7)*

**Files:** `epoch_pg/src/event_bus/mod.rs`, `epoch_pg/tests/pgeventbus_integration_tests.rs`, `epoch_pg/tests/saga_adapter_integration_tests.rs`, `CHANGELOG.md`, `specs/0024-cloud221-subscriber-readiness.md`.
