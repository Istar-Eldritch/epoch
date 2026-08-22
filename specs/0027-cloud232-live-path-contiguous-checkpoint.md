# Spec 0027: Live-Path Contiguous Checkpoint

**Issue:** CLOUD-232 · **Status:** Implemented · **Crate:** `epoch_pg` · **Scope:** `fix(pg)`, no migration/schema/public-API change · **Sequel to:** spec 0026 (CLOUD-226).

## 1. Problem

`epoch_events.global_sequence` uses non-transactional `nextval()` (spec 0019), so a reader can see 5 and 7 while 6 is held by an open transaction (a **hole**). Persisting a checkpoint above a hole means the holding transaction's event, once committed, sits below the checkpoint and is delivered to nobody, with no WARN/DLQ/gap record. Spec 0026 closed this for `catch_up_from_checkpoint` and the `subscribe()` drain but **scoped out the live listener path**, where the same failure survives. It is P1 because per-subscriber state is seeded from the *persisted* checkpoint on wake (`start_listener`), so a poisoned checkpoint becomes the next boot's seed: permanent loss.

### 1.1 Mechanism (all in `epoch_pg/src/event_bus/`)

1. `process_subscriber_for_batch` updated `pending_checkpoint` with **every processed event's own sequence** (deser-skip and success paths) via `PendingCheckpoint::update` — max-seen, not contiguous.
2. It was corrected to contiguous only inside `if new_contiguous > contiguous_before`, by direct field assignment.
3. So a batch containing a held hole where the prefix does not advance leaves `pending_checkpoint = Some(max_seen)`, above the hole.
4. The value is carried across batches via the `pending_checkpoints` map until flushed.
5. Two flush sites had **no contiguity check**: `flush_all_pending_checkpoints` (reconnect + shutdown, all modes) and `flush_expired_checkpoints` (`Batched` only, per `max_delay`). Both violate `flush_checkpoint`'s documented contract.

### 1.2 Severity

Default `checkpoint_mode` is `Synchronous`, and `flush_expired_checkpoints` early-returns for it. So under the default the leak fires only on **reconnect/shutdown** (rolling restart under write load). Under `Batched` it fires **routinely**, every `max_delay_ms`.

## 2. What must NOT break

- `advance_contiguous_checkpoint` (`subscriber_state.rs`) is the authoritative, correct advancer: it skips an abandoned hole when the fence is provably cleared (`snapshot.xmin >= fence_xmax`) or the `gap_timeout` backstop fires, else holds. No permanent stall, because `WakeReason::TimerTick` (1s `flush_interval`) re-enters the drain and the batch query `global_sequence > min_checkpoint` stays non-empty while anything sits above the hole.
- **The two advancers stay separate.** `advance_catchup_prefix` is a fence-less single-event exact-match advance by design (its pagination cursor cannot re-observe a gap). Merging was rejected.
- `try_flush_pending_checkpoint` is already correctly shared; only its call-site placement changes.
- `update_checkpoint` (public deliberate-rewind API) never populates `pending_checkpoints`, so a monotonic `WHERE` guard on `flush_checkpoint` was rejected. Untouched.

## 3. Root defect and design

### 3.1 The conflation

`PendingCheckpoint` served two jobs: `global_sequence`/`event_id` = position to publish; `events_since_checkpoint` = `Batched` `batch_size` threshold; `first_event_time` = `max_delay_ms` threshold. `update()` conflated moving the position with bumping the counter. Naively deleting the per-event `update()` calls would freeze the counter at 1 and kill the `batch_size` trigger.

### 3.2 Design: publish the contiguous position, count every processed event

1. Added `PendingCheckpoint::record_processed` — bumps `events_since_checkpoint` only.
2. Both per-event sites call `record_processed`, seeding the pending on first use from `state.contiguous_checkpoint` and its paired id (stable across the row loop).
3. The contiguous branch calls `PendingCheckpoint::advance()` (a raw position/id move that leaves the counter untouched), NOT `update()`, so it records the paired `event_id` without bumping the counter. Using `update()` here would double-count: `record_processed` already counted each event at its per-event site, so bumping again on the same events at the advance would inflate `events_since_checkpoint` past the actual number of unflushed events.
4. **Moved `try_flush_pending_checkpoint` out of `if new_contiguous > contiguous_before`** so it runs unconditionally, gated on `!replay_always`. Without this a `Batched` `batch_size` crossing could not flush while a hole was open.

Eager seeding (not `Option<u64>`) keeps the field non-optional and preserves `max_delay_ms` measuring from the first unflushed event. The seed itself is **not publishable** (§4 Q1): while a hole is held the pending never publishes, and the counter/timer keep accruing, bounded by `gap_timeout`.

### 3.3 The invariant

> **After a flush from any site, the persisted checkpoint MAY lag `state.contiguous_checkpoint`; it MUST NEVER lead it.**

Regression tests pin "never leads"; positive controls pin "the lag is bounded and eventually closes".

### 3.4 Call-site details

- `try_flush_pending_checkpoint` uses `take_if(...)`, so a sub-threshold call leaves the pending intact — moving it out of the branch is safe w.r.t. the carried-across-batches contract.
- `cached_checkpoint` set **only from `local_cache`** (only when a flush happened); the unconditional `cached_checkpoint = Some(new_contiguous)` on advance is **deleted**. Behaviour change, not a refactor; mitigated by the cache having no readers.
- The new flush must sit inside `!replay_always`, not rely on the later nulling, or a `ReplayAlways` subscriber writes a row before it is cleared.
- **Publishable predicate goes inside the selection**: fold into the `take_if` closure and into the `filter`/key choice of both bulk flushers. Applying it after `take_if`/`remove` silently destroys the pending and loses `first_event_time`/`events_since_checkpoint` (R4 damage).
- The `None` arm of the contiguous branch survives as the publishable advance constructor: reachable when every row hits the `processed_ahead` early-continue and the gap is then closed by `advance_contiguous_checkpoint`.

## 4. Decided open questions

### Q1 — seed's paired `event_id`. **Add the field.**

`SubscriberState` had no paired event id; the seeding query selected only `last_global_sequence`. Alternatives rejected: `seq_to_id.get(contiguous_before)` **structurally** misses (the batch query excludes the row at the min checkpoint); the listener's `last_event_ids` map is polluted by ahead-of-hole events and empty on boot; `Uuid::nil()` violates spec 0026 R4.

Implemented: added `contiguous_event_id: Uuid` to `SubscriberState`; added `last_event_id` to the seeding `SELECT` (`(i64, Option<Uuid>)`, `None → nil`); kept `SubscriberState::new(checkpoint)`, added `new_with_event_id` for the production site. `process_subscriber_for_batch` sets the field on advance.

**Publishable predicate (amended):** added `seed_sequence: u64`; a pending is publishable only when `global_sequence > seed_sequence`. Subsumes the earlier `> 0` rule: a fresh subscriber (seeds at 0) never writes a sequence-0 row, and an existing subscriber never re-writes its own seed (so a NULL `last_event_id` is never degraded to nil). Answerable from `PendingCheckpoint` alone.

**The trap:** the **eager seed** sets `seed_sequence` to its own `global_sequence` (born unpublishable); the **advance** path (`PendingCheckpoint::new`, the `None` arm) sets `seed_sequence` **below** its position (born publishable). Two distinct named constructors, both unit-tested. `PendingCheckpoint::new` becomes the advance constructor; the new constructor is the eager seed.

### Q2 — suppress redundant idempotent writes? **Out of scope** (the narrower redundant-write case is already eliminated by Q1's predicate).

### Q3 — guard or trust producers? **`debug_assert!` only.** `debug_assert!(p.global_sequence <= state.contiguous_checkpoint)` before the flush. Not the rejected monotonic `WHERE` clause.

### Q4 — `ReplayAlways`. **Preserve, as R6.** The new flush is inside `!replay_always`; unit-test guarded.

## 5. Requirements

- **R1.** Live path MUST NOT persist a checkpoint above an unproven-contiguous sequence, including via both bulk flushers.
- **R2.** A rolled-back-transaction hole MUST still be skipped via both fence-cleared and timeout-backstop routes; no permanent stall.
- **R3.** `Batched` `batch_size` trigger stays functional, including while a hole is held.
- **R4.** `max_delay_ms` keeps measuring from the first unflushed event.
- **R5.** `last_event_id` MUST be no worse paired than the existing advance path (spec 0026 R4); a never-advanced seed MUST NOT be published. (The pre-existing fallback mismatch on hole-skip is out of scope; the seed introduces no *new* mismatch.)
- **R6.** `ReplayAlways` subscribers write no checkpoint from this path.
- **R7.** No public API / error-type / schema / migration change. All touched items are `pub(crate)` or private.

## 6. Behavioural risk owned

- Under **`Synchronous` (default)**: behaviour changes only at the **shutdown/reconnect boundary**; steady-state readiness unaffected (persisted value already held at the hole).
- Under **`Batched`**: reaches steady state — a subscriber over an open hole now reports not-caught-up until the hole closes, **bounded by `gap_timeout`** (default 5s).

CHANGELOG scoped accordingly: silent loss closed on shutdown/reconnect in **all** modes and on the `Batched` timer tick; the readiness delay attributed **only to `Batched`**. Do not claim a steady-state change on the default mode.

## 7. Test plan (implemented)

### Decisions
- **Private events table per test** via `events_table` + `setup_trigger` before `start_listener` (`LIKE` doesn't copy triggers), `Uuid`-unique channel. Enables exact-equality assertions and per-test checkpoint/gap rows; no `truncate_epoch_tables`, avoiding `ACCESS EXCLUSIVE` contention. Does **not** grant immunity from sibling `TRUNCATE` on the shared checkpoint tables, but converts that from a silent pass to a loud failure.
- **Assert exact equality**, never `< seq_hole` (which passes when nothing was written). Commit an event *below* the hole to assert against.
- **Ordering:** observe, then roll back/commit and shut down, then assert. Stay well inside the 60s idle-in-transaction timeout.

### Prerequisite (blocker-grade)
`claim_hole_uncommitted`, `insert_committed_event`, and their caller `create_sequence_gap` hardcoded `INSERT INTO epoch_events` — needed a `table` param, else the hole burns the shared sequence and regression tests pass vacuously. `PgEventStore::new` hardcodes `epoch_events` → use `PgEventStore::with_table` (also ensures the `txid` column).

### Regression tests (R1)
1. **`test_live_shutdown_does_not_publish_above_held_hole` — PRIMARY.** Default `Synchronous`; `shutdown()` with the hole open; assert persisted checkpoint **equals** `seq_below` **and** persisted `last_event_id` equals the below-hole id (the only DB-level R5 check).
2. **`test_live_batched_flush_does_not_publish_above_held_hole`.** `Batched { batch_size: 1000, max_delay_ms: 300 }`; one unavoidable fixed sleep `>= 2x` the 1s `flush_interval`, taken *after* delivery is verified (must not become a poll loop — no positive edge to poll).
3. **`test_live_deser_skip_above_hole_does_not_publish_above_hole`.** Raw inserts with valid JSON that is not a known variant (`{"NoSuchVariant":{}}`, not garbage bytes which jsonb rejects); delivery poll impossible, so `captured_logs_contain_since` on the WARN is the substitute signal; names its mode and flush trigger.

Plus a `ReplayAlways` guard (R6).

### Positive controls (no stall)
- **`test_backstop_hole_still_advances_checkpoint` — MANDATORY.** `snapshot_fencing: false`, `gap_timeout: 500ms`, zero load sensitivity; asserts checkpoint `>= seq_above` and a gap-timeout row exists. (R2)
- **`test_fence_cleared_hole_still_advances_checkpoint` — shipped `#[ignore]`d.** `snapshot_fencing: true`, `gap_timeout: 30s`, asserts no gap-timeout row. Load-sensitive (instance-wide `xmin`); redundant with the backstop control and `test_rolled_back_gap_fence_clears_without_record`. (R2)
- **Hardened `test_batched_checkpoint_flushes_at_batch_size` / `_at_max_delay`** to exact-value on isolated table. (R3, R4)
- **Hardened `test_synchronous_checkpoint_still_works`** to exact-value, storing 2–3 events to prove cross-batch cadence on the default mode. (R3)
- **`checkpoint.rs` unit tests:** `record_processed` bumps only the counter; `update` does both jobs; both constructors pinned (eager seed unpublishable, advance publishable). (R3, R5)
- **`ReplayAlways` unit guard.** (R6)

### Accepted gaps
- Reconnect flush untested — same function/guard as shutdown (test 1), only the trigger differs.
- `inline_dispatch` / `transaction` integration tests inferred unaffected (don't go through `process_subscriber_for_batch`), not verified.
- Known load-sensitive existing tests flagged: `test_in_flight_transaction_gap_is_held`, `test_rolled_back_gap_fence_clears_without_record`.

## 8. Phasing (as delivered)

- **P1** `test(pg): exact-value and no-stall controls for live checkpointing` — isolated-table infra, `table` params, `with_table` conversion, hardened controls, backstop control live, fence-cleared `#[ignore]`d. (`04568437`)
- **P2** `fix(pg): publish only contiguous checkpoints from the live listener path` — `record_processed`, `seed_sequence`, two constructors, predicate + unit tests; `SubscriberState.contiguous_event_id` + seeding `SELECT`; per-event sites → `record_processed`; contiguous branch → `advance()` (NOT `update()`, which would double-count events already counted by `record_processed`); flush moved out of the advance branch under `!replay_always`; `cached_checkpoint` from `local_cache` only; `debug_assert`; predicate inside all three selections. (`5657e69c`; spec-correction `83bc5b93`)
- **P3** `test(pg): pin the live path against publishing above a held hole` — the three regression tests + `ReplayAlways` guard; verified failing against P2 reverted. (`55787f37`, plus `c1ad2269` wiring event IDs into state verification)
- **P4** `docs(pg): record the live-path contiguous checkpoint fix` — scoped CHANGELOG entry, rustdoc on new `pub(crate)` items, `flush_checkpoint` hazard doc cites 0027 R1, spec marked Implemented. (`5911caa9`)

> No standalone "add primitives" phase: a `pub(crate)` item with zero callers is `dead_code` under `clippy --all-targets -D warnings`, so primitives land with their call sites in P2.

## 9. Acceptance criteria

1. New tests pass; existing gap-fence/gap-timeout tests pass modulo P1 hardenings.
2. Reverting P2 fails §7 regression tests 1–3, verified by stashing.
3. `cargo fmt --check` clean; `cargo clippy --all-targets -p epoch_pg -p epoch_core -- -D warnings` clean. Do **not** gate on workspace-wide clippy (pre-existing `ambiguous_glob_reexports` at `epoch/src/lib.rs`, out of scope).
4. Event-bus binary passes in isolation and under a concurrent writer, on consecutive runs.
5. No new `unwrap()`/`expect()` in library code; new `pub(crate)` items carry rustdoc.
6. `git diff --stat` shows no migration and no `pub`-signature change.

## 10. Out of scope

- Merging `advance_catchup_prefix` with `advance_contiguous_checkpoint` (rejected).
- Monotonic `WHERE` guard on `flush_checkpoint` (breaks deliberate rewind).
- Skipping flushes equal to the last *cached* persisted value (narrower redundant-write case already eliminated by Q1).
- Making `flush_interval` configurable.
- CLOUD-227, CLOUD-231; testing the reconnect flush trigger.

## 11. Residual uncertainty

- **Lost-outcome direction:** a task that never returns drops its in-memory pending. Inferred to fail *safe* (checkpoint not advanced rather than over-advanced), not proven by test.
- **Freshness regression (accepted cost of Q1 amendment):** after a lost outcome the next batch seeds above the persisted value and the predicate refuses to publish, so the persisted checkpoint stays stale until the prefix advances again. This is a lag (permitted by §3.3, resolved by at-least-once redelivery), not a correctness bug.
- **Fence-cleared control's real flakiness rate** unknown; shipped `#[ignore]`d, so a fence-route regression would rest on `test_rolled_back_gap_fence_clears_without_record` alone.
- **`cached_checkpoint` sourcing change has no dedicated test** — an unpublishable pending now yields `None` where it was `Some(new_contiguous)`; intended, and the map has no readers, but untested. Most likely source of surprise.
