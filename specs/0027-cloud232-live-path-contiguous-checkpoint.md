# Spec 0027: Live-Path Contiguous Checkpoint

**Issue:** CLOUD-232 · **Status:** Proposed · **Crate:** `epoch_pg`
**Scope:** `fix(pg)` — no migration, schema, or public-API change.
**Sequel to:** spec 0026 (CLOUD-226), same problem domain and vocabulary.
**Input:** `specs/briefs/0027-cloud232-live-path-contiguous-checkpoint-brief.md` (discovery, verified on `main`).

> Anchors are symbol names. Line numbers, where given, are parenthetical hints that will drift.
> Every symbol named below was grepped and resolves on `main` at `152f551`.

## 1. Problem

`epoch_events.global_sequence` comes from a non-transactional `nextval()` (spec 0019), so a reader can see 5 and 7 while 6 is claimed by an open transaction. That missing sequence is a **hole**. Persisting a checkpoint above a hole means the holding transaction's event, once committed, sits below the checkpoint and is delivered to nobody: no WARN, no DLQ row, no gap-timeout record, and readiness still reports success.

Spec 0026 closed this for `catch_up_from_checkpoint` and the `subscribe()` buffer drain. **The same failure mode survives on the live listener path**, which 0026 scoped out. The guarantee is therefore not end-to-end: a bus catches up conservatively and then has the bad value written underneath it moments later.

Why P1 rather than a missed-delivery annoyance: per-subscriber state is seeded from the **persisted** checkpoint when the listener wakes (`start_listener`, the `SELECT last_global_sequence` into `SubscriberState::new`, ~mod.rs:1412). A poisoned checkpoint becomes the next boot's seed, so the loss is permanent, not transient.

### 1.1 Verified mechanism

All in `epoch_pg/src/event_bus/`.

1. `process_subscriber_for_batch` (`mod.rs`) updates `pending_checkpoint` with **every processed event's own sequence**, unconditionally, on both the deserialization-skip path (~283) and the success path (~321), via `PendingCheckpoint::update`. That is max-seen, not contiguous.
2. It is corrected to contiguous only inside `if new_contiguous > contiguous_before` (~470), and there by **direct field assignment** (`p.global_sequence = …; p.event_id = …`), not `update()`.
3. So when a batch contains a held hole and the prefix does not advance, that branch never runs and `pending_checkpoint` survives as `Some(max_seen)`, above the hole.
4. The value is returned in `SubscriberBatchOutcome` and stored in the listener's `pending_checkpoints` map, and is **carried across batches** (`pending_checkpoints.remove(sid)` fed back in as a parameter). A stale max-seen value persists until something flushes it.
5. Two sites write it, both calling `flush_checkpoint` with **no contiguity check**:
   - `flush_all_pending_checkpoints` (`checkpoint.rs`): no mode check either. Runs on **reconnect and shutdown, in every checkpoint mode**.
   - `flush_expired_checkpoints` (`checkpoint.rs`): early-returns unless `Batched`, then flushes any entry whose `first_event_time.elapsed() >= max_delay`.

`flush_checkpoint`'s own rustdoc states the contract these two violate: "Every caller MUST pass only a value it is willing to publish as the subscriber's current position."

### 1.2 Severity, refined

`ReliableDeliveryConfig::default()` sets `checkpoint_mode: Synchronous` (`config.rs`, ~259) and `flush_expired_checkpoints` early-returns for `Synchronous`. So:

- Under **`Synchronous` (the default)** the leak fires only on **reconnect or shutdown** — i.e. a rolling restart under write load, the real incident shape.
- Under **`Batched`** it fires **routinely**, every `max_delay_ms`.

The exposure window is not "a batch that races a hole" but "any batch that ever processed above a held hole, until the next flush of any kind".

### 1.3 Two corrections to the Linear ticket

- The ticket warns that publishing only the contiguous value can **permanently stall** a subscriber on a rolled-back transaction. It cannot. The skip logic lives *inside* `advance_contiguous_checkpoint` (§2), not beside it.
- The ticket implies the live and catch-up advancers should converge. They must not (§2, and a prior review round rejected it).

## 2. What is NOT wrong, and must not be broken

`advance_contiguous_checkpoint` (`subscriber_state.rs`) is the authoritative advancer of `state.contiguous_checkpoint` and is correct. It already folds in the deliberate skip of an abandoned hole: it walks past a gap when the fence is provably cleared (`snapshot.xmin >= fence_xmax`, proving the writer aborted) or when the `gap_timeout` backstop fires, and otherwise holds.

`try_flush_pending_checkpoint` (`mod.rs`) is already correctly shared by the catch-up and live paths and needs no change beyond its call-site placement (§3.4). The defect is entirely in what the live path feeds it.

**The two advancers must stay separate.** `advance_catchup_prefix` is a single-event exact-match (`seq == contiguous + 1`) advance with no fence, no gap timeout and no `processed_ahead` set, by design (spec 0026 §3, whose rejection argument is that catch-up's pagination cursor structurally cannot re-observe a gap, so a fence verdict there would silently drop a *committed* event). `advance_contiguous_checkpoint` batches, fences via `TxidSnapshot`, and times out stuck gaps. Do not propose merging them; this has already been rejected once.

`update_checkpoint` (`mod.rs`, ~1720) is the **public** deliberate-rewind API and is why a blanket monotonic `WHERE` guard on `flush_checkpoint` was rejected previously. It never populates `pending_checkpoints`, so it is untouched here.

## 3. Root defect and design

### 3.1 The conflation

`PendingCheckpoint` (`checkpoint.rs`) has four fields serving **two jobs**:

| Field | Job | Written by |
| --- | --- | --- |
| `global_sequence`, `event_id` | the position to publish | `new()`, `update()`, direct assignment |
| `events_since_checkpoint` | `Batched` `batch_size` threshold | `update()` only |
| `first_event_time` | `Batched` `max_delay_ms` threshold | `new()` only, never refreshed |

`update()` conflates them: it moves the published position *and* bumps the counter. The per-event calls exist for the accounting; moving the position is collateral damage.

**Why the naive deletion is wrong.** Simply deleting the two per-event `update()` calls leaves nothing incrementing `events_since_checkpoint`, because the contiguous branch uses direct assignment. The counter would freeze at 1, the `batch_size` trigger would go permanently dead, and `Batched` would silently degrade to `max_delay_ms`-only. Not data loss, but it defeats the feature.

### 3.2 Design: publish the contiguous position, count every processed event

1. `checkpoint.rs`: add `PendingCheckpoint::record_processed(&mut self)`, which bumps `events_since_checkpoint` and touches nothing else. Ahead-of-gap events must still count toward the `Batched` threshold, since they will be redelivered from whatever is persisted, but must never *become* the published value.
2. Both per-event sites in `process_subscriber_for_batch` call `record_processed` instead of `update`, seeding the `PendingCheckpoint` on first use from `state.contiguous_checkpoint` and its paired id (§4 Q1). `state.contiguous_checkpoint` is stable for the whole row loop — it is mutated only later, by `advance_contiguous_checkpoint` — so it is a safe seed at any point.
3. The contiguous branch switches from direct assignment to `update()`, so an advance bumps the counter too and records the paired `event_id` for the next seed.
4. **Move the `try_flush_pending_checkpoint` call out of `if new_contiguous > contiguous_before`** so it runs unconditionally, still gated on `!replay_always`. Today the only call site sits inside the branch that a held hole prevents from running, so a `Batched` `batch_size` crossing cannot flush while a hole is open. Without this move the rest of the fix is cosmetically correct but the §3.1 cadence regression persists.

**Why eager seeding rather than `Option<u64>` for the position.** There is always a legitimately publishable contiguous value: the current one, usually equal to what is already persisted, so re-publishing it is idempotent. That keeps the field non-optional, needs no `None`-skipping in either flush site, and preserves `max_delay_ms` measuring from the first unflushed event rather than restarting at the first advance. An earlier `Option<u64>` proposal was dropped for this reason.

**Counter self-bounds.** With eager seeding, a threshold trip while a hole is held flushes the seed (a harmless idempotent write of the already-persisted value) and resets the counter. The cost is periodic redundant writes for as long as the hole is held, not unbounded growth.

### 3.3 The invariant, which is the spine of §5

> **After a flush from any site, the persisted checkpoint MAY lag `state.contiguous_checkpoint`; it MUST NEVER lead it.**

Lagging is legitimate and expected: between flushes, and under `Batched` by design. Leading is the bug. Every requirement below is a consequence of this asymmetry, and the tests in §7 pin both directions of it — the regression tests pin "never leads", the positive controls pin "the lag is bounded and does eventually close".

### 3.4 Call-site placement details the implementer will hit

- `try_flush_pending_checkpoint` uses `take_if(|p| should_flush_checkpoint(p, mode))`, so a call that does not meet the threshold leaves `pending_checkpoint` intact. Moving the call out of the branch is therefore safe with respect to the carried-across-batches contract.
- `cached_checkpoint` is currently set inside the same branch (`Some(new_contiguous)`, then overwritten from `local_cache`) and is merged into the listener cache by the caller (~1602). After the move, set it from `local_cache` whenever a flush actually happened, and keep setting it to `Some(new_contiguous)` when the prefix advanced. It is a cache of the persisted value; it must never be set to a value that was not written.
- `process_subscriber_for_batch` nulls both `pending_checkpoint` and `cached_checkpoint` for a `ReplayAlways` subscriber *after* the branch. The new unconditional flush must sit inside a `!replay_always` guard, not merely rely on that later nulling, or a `ReplayAlways` subscriber would write a checkpoint row before it is cleared.

## 4. Settling the brief's open questions

The brief left four open. All four are decided here. **Q1 is the only one I want a human on**; Q2–Q4 are safe to take as specified.

### Q1 — where the seed's paired `event_id` comes from. **DECISION: add the field. NEEDS SIGN-OFF.**

Verified current shape: `SubscriberState` (`subscriber_state.rs`) has exactly three fields — `contiguous_checkpoint: u64`, `processed_ahead: HashSet<u64>`, `gap_first_seen: HashMap<u64, GapObservation>` — and **no paired event id**. The seeding query in `start_listener` selects **only** `last_global_sequence`. `epoch_event_bus_checkpoints.last_event_id` is nullable `UUID` (`m003_create_event_bus_infrastructure.rs`, ~66).

Spec 0026 **R4** requires `last_event_id` to match `last_global_sequence`. Eager seeding therefore needs an id, and the alternatives are worse:

- `seq_to_id.get(&contiguous_before)` is unreliable: the shared batch fetches from `min_checkpoint` across all subscribers, so the row *at* this subscriber's contiguous position is frequently not in the batch. Silently violates R4.
- Seeding `Uuid::nil()` and hoping a flush never happens before the first advance is exactly the case the `Batched` threshold makes reachable. Violates R4 in the field.

So: add `contiguous_event_id: Uuid` to `SubscriberState`, and add `last_event_id` to the seeding `SELECT` (now `query_as::<_, (i64, Option<Uuid>)>`, mapping `None` to `Uuid::nil()`). Keep `SubscriberState::new(checkpoint)` as-is (21 call sites, mostly unit tests in `subscriber_state.rs`) seeding `contiguous_event_id: Uuid::nil()`, and add `SubscriberState::new_with_event_id(checkpoint, event_id)` for the production seeding site. `advance_contiguous_checkpoint` does not need to maintain the field; `process_subscriber_for_batch` sets it alongside `checkpoint_event_id` when the prefix advances.

**The sequence-0 sub-question, decided:** a brand-new subscriber seeds `(0, nil)`, and flushing that would create a checkpoint row at sequence 0 where none existed, with a nil `last_event_id` that corresponds to nothing. A 0 row and a missing row both mean "replay from the start", so it is *harmless*, but it is also *meaningless* and it makes R4 formally false. **Do not write it.** Add one shared predicate — the pending is publishable only when `global_sequence > 0` — and apply it in `try_flush_pending_checkpoint`, `flush_expired_checkpoints` and `flush_all_pending_checkpoints`. This is a guard on genuinely reachable state (a fresh subscriber), not on structurally-impossible state, so it is not the thing CLAUDE.md discourages; contrast Q3. Consequence to accept: for a brand-new subscriber blocked at a hole on sequence 1, `events_since_checkpoint` grows unflushed for the hole's lifetime, bounded by `gap_timeout`.

**Why this needs sign-off:** it is the only part of the change that grows a struct and a query, i.e. the only part with a shape a reviewer might want done differently. If the reviewer prefers not to touch `SubscriberState`, the only sound fallback is `Option<Uuid>` on the *pending* position with `None`-skipping in all three flush sites, which is the design §3.2 argues against. Say so before implementing, not after.

### Q2 — suppress redundant idempotent writes? **DECISION: out of scope.** Safe to decide

`try_flush_pending_checkpoint` could skip when the pending position equals the cached persisted value, using the existing `checkpoint_cache`. Reasons to leave it: the write is a single indexed upsert on a tiny table, occurring at most once per `Batched` threshold trip per subscriber while a hole is held; and it interacts with `cached_checkpoint` merge semantics (§3.4) in a way that adds review surface to a P1 correctness fix. Note also that `process_subscriber_for_batch` deliberately uses a *fresh local cache*, not the listener-level one, so the comparison would be against an empty map there and would not fire anyway without further plumbing. File as a follow-up if the redundant writes ever show up in `pg_stat_statements`.

### Q3 — guard, or trust the producers? **DECISION: `debug_assert!` only.** Safe to decide

Once the two producers are fixed, every value that can occupy `pending_checkpoints` is provably contiguous, so a runtime check would validate something that structurally cannot happen. Add `debug_assert!(p.global_sequence <= state.contiguous_checkpoint)` in `process_subscriber_for_batch` immediately before the flush call, so a future refactor that reintroduces a max-seen write trips in test builds.

Record the distinction explicitly: this is **not** the previously rejected proposal. That one was a monotonic `WHERE EXCLUDED.last_global_sequence > last_global_sequence` clause on `flush_checkpoint` itself, rejected because it would break the legitimate deliberate-rewind feature reachable through the public `update_checkpoint`. A `debug_assert` on the live path does not touch that path.

### Q4 — `ReplayAlways`. **DECISION: preserve, and make it R6.** Safe to decide

Confirmed against source: `process_subscriber_for_batch` nulls `pending_checkpoint` and `cached_checkpoint` for `ReplayAlways` after the contiguous branch, so the listener's periodic and shutdown flushes cannot defeat replay-from-zero. The new unconditional flush call site must be inside `!replay_always` (§3.4), which preserves it. This becomes R6 and gets a unit-test guard.

## 5. Requirements

- **R1.** The live path MUST NOT persist a checkpoint above a sequence it has not proven contiguous, including via `flush_expired_checkpoints` and `flush_all_pending_checkpoints`. (The §3.3 invariant: persisted may lag `contiguous_checkpoint`, never lead it.)
- **R2.** A hole abandoned by a rolled-back transaction MUST still be skipped, via **both** the fence-cleared and timeout-backstop routes, so no subscriber stalls permanently.
- **R3.** `Batched`'s `batch_size` trigger MUST remain functional, including while a hole is held. (Guards against the §3.1 counter regression.)
- **R4.** `max_delay_ms` MUST continue to measure from the first unflushed event, not from the first prefix advance.
- **R5.** A persisted checkpoint's `last_event_id` MUST match its `last_global_sequence` (spec 0026 R4), including for the eagerly-seeded value.
- **R6.** `ReplayAlways` subscribers MUST continue to write no checkpoint from this path.
- **R7.** No public API change, no error-type change, no schema change, no migration (spec 0026 R7). `PendingCheckpoint`, `SubscriberState` and `process_subscriber_for_batch` are all `pub(crate)` or private; `CheckpointMode`, `should_flush_checkpoint` and the checkpoints table are untouched. **If the implementer concludes any of these must change, stop and raise it before writing code** — it invalidates the scope of this spec.

## 6. Behavioural risk this change owns

This is a first-class consequence, not a footnote.

Spec 0026 **R5** already changed readiness semantics for the catch-up pass: a gate that used to report caught-up over a lost event now blocks. Clamping the live path extends that into **steady-state operation**. A subscriber sitting behind an open transaction will now report not-caught-up for as long as that transaction lives, **bounded by `gap_timeout`**, after which the backstop skips the hole and the prefix advances. Previously the bound was effectively the flush interval, because the checkpoint jumped the hole.

That is the correct behaviour — the alternative is the silent loss this ticket exists to fix — but it is user-visible, not internal. Consequences:

- Any caller with a tight `wait_until_caught_up` bound may newly time out while a long-running writer holds a transaction open. Existing bounds in the suite look generous (15s), but this is the mechanism to watch when the fix lands.
- It requires a **CHANGELOG entry** mirroring the 0026 R5 one, under `### Fixed`, stating both the loss that is closed and the readiness delay bounded by `gap_timeout`. A bare bug-fix line is not sufficient.

## 7. Test plan

Priorities are the brief's §8 and are deliberately counterintuitive. Do not reorder them.

### 7.1 Two decisions that shape everything else

**Use a private events table per test.** `ReliableDeliveryConfig::events_table` is public and `setup_trigger` (`mod.rs`, ~933) supports a custom table. The unit-test helper `cu_isolated_events_table` (`mod.rs`, `#[cfg(test)]`, ~3717) does `CREATE TABLE … (LIKE epoch_events INCLUDING ALL)` plus its **own sequence**; the integration binary needs its own copy, since that helper is not reachable from `tests/`. `LIKE` does **not** copy triggers, so `setup_trigger()` must run before `start_listener()`, with a `Uuid`-unique channel name.

This buys three things: the sequence is no longer shared, so **exact-equality** assertions become possible; `bus_name` is `config.events_table`, so checkpoint and gap-timeout rows are per-test; and no `truncate_epoch_tables` call is needed, so these tests contribute nothing to the `ACCESS EXCLUSIVE` contention that previously wedged this project for five hours.

**Assert exact equality, never `< seq_hole`.** The existing near-miss pattern `checkpoint.is_none() || matches!(checkpoint, Some(s) if s < seq_n)` **passes when nothing was ever written**, including when a sibling binary's `TRUNCATE` wipes the row mid-test. Committing an event *below* the hole first gives a known-good value to assert equality against, which simultaneously proves the leak is closed, proves the write path is alive, and proves the fix did not move the checkpoint backwards. **This is a review gate for this spec.**

**Ordering discipline, inherited from CLOUD-226 and mandatory:** capture the observation, then roll back or commit the held transaction and shut down, **then** assert. An `assert!` that unwinds with the transaction still open is exactly the shape that produced the wedge. Test pools set `lock_timeout = '30s'` and `idle_in_transaction_session_timeout = '60s'`, so any test holding a hole must stay well inside 60s.

### 7.2 Regression tests (R1)

All share: isolated table; `gap_timeout: 30s` so the backstop cannot fire inside the window; a committed event *below* the hole; `claim_hole_uncommitted` (`pgeventbus_integration_tests.rs`, ~5605) to open the hole; two committed events above it; and a **bounded poll** proving the above-hole events were delivered, which is what proves `pending_checkpoint` was populated with max-seen in the first place.

1. **`test_live_shutdown_does_not_publish_above_held_hole` — PRIMARY.** Default `Synchronous`, stated explicitly in a comment, since the whole point is that only the shutdown flush can leak on the production default. `shutdown()` with the transaction still open, then read. Asserts the persisted checkpoint **equals** `seq_below`. Pre-fix this reads `Some(seq_above2)`.
2. **`test_live_batched_flush_does_not_publish_above_held_hole`.** Targets `flush_expired_checkpoints` with `Batched { batch_size: 1000, max_delay_ms: 300 }` — `batch_size` high so only the delay can fire.
3. **`test_live_deser_skip_above_hole_does_not_publish_above_hole`.** The deserialization-skip branch is the **second** unconditional write site and no existing test covers it above a hole. Write the above-hole events as raw inserts with garbage `data` so deserialization fails, and assert via `common::captured_logs_contain_since` that the WARN actually fired — otherwise the test is vacuous.

**One fixed sleep is unavoidable in test 2, and a reviewer must not "clean it up" into a poll loop.** "Checkpoint has not advanced" is both the expected post-condition and what a stalled listener looks like, so there is no positive edge to poll for; a poll loop here cannot fail. Mitigate by verifying delivery *before* the sleep starts, so the sleep only has to cover the flush tick, and size it at `>= 2x flush_interval`. Note `flush_interval` is a hard-coded `Duration::from_secs(1)` in `start_listener` (~1237), so every timer-tick test is floored at ~1s; making it configurable is a public-behaviour change and out of scope.

### 7.3 Positive controls: the fix must not stall a subscriber

These must land no later than the phase that changes flushing behaviour (§8), so a stalling regression cannot pass unnoticed.

- **`test_backstop_hole_still_advances_checkpoint` — MANDATORY, and the one to trust.** With `snapshot_fencing: false` and `gap_timeout: 500ms` it depends only on elapsed wall-clock, so it has **zero load sensitivity**. Asserts the checkpoint reaches `>= seq_above` (`>=`, since after a backstop skip the prefix legitimately runs to head) and that a gap-timeout row exists. (R2)
- **`test_fence_cleared_hole_still_advances_checkpoint`.** Same shape with `snapshot_fencing: true`, `gap_timeout: 30s` so a pass cannot be the backstop in disguise, asserting **no** gap-timeout row. **Load-sensitive, and an isolated table does not help**, because clearing needs `snap.xmin >= fence_xmax` and `xmin` is **instance-wide**: any transaction open anywhere on the server pins it, and sibling tests in this very suite hold transactions for 4s+. Also, `batch_snapshot` is only queried once a gap is already active, so `fence_xmax` is `None` on first observation and backfilled on the next batch, giving a two-tick (~2s) floor. **It should fail, not report inconclusive**: an inconclusive positive control cannot distinguish a stalling fix from a working one. It is paired with the backstop control so one load-insensitive hard signal always remains. (R2)
- **Harden `test_batched_checkpoint_flushes_at_batch_size` and `test_batched_checkpoint_flushes_at_max_delay`** from `assert!(checkpoint.is_some())` to exact-value assertions on an isolated table. This is the control that catches the §3.1 counter regression, and it converts two currently decorative tests into real ones. (R3, R4)
- **Unit tests in `checkpoint.rs`:** `record_processed` bumps the counter and leaves `global_sequence`/`event_id`/`first_event_time` untouched; `update` still does both jobs; the publishable predicate skips `global_sequence == 0` and passes anything above it. No database, and they will still mean something in five years. (R3, R5)
- **Unit test in `subscriber_state.rs` or `mod.rs`:** `ReplayAlways` regression guard — no checkpoint written from the live path. (R6)

### 7.4 Reconnect is a knowingly-accepted gap

The reconnect flush is reached only from `WakeReason::Notification(Err(e))`, which needs the listener's `PgListener` connection killed externally via `pg_terminate_backend`. That is racy to target and is a cross-connection kill on a shared instance. It is also redundant: it is the **same function with the same missing guard** as the shutdown path, so test 1 covers the defect and only the trigger differs. Recorded here deliberately, not overlooked.

### 7.5 Existing tests

**No existing test encodes the bug as expected**, verified across `pgeventbus_integration_tests.rs` and `saga_adapter_integration_tests.rs`. The four `assert!(checkpoint.is_some())` tests are insensitive in both directions. The two lower-bound assertions (`test_in_flight_transaction_gap_is_held` post-commit, `test_rolled_back_gap_fence_clears_without_record`) are cases where the prefix legitimately reaches the asserted value, and the second doubles as an existing positive control.

Not audited: `inline_dispatch_integration_tests.rs`, `transaction_integration_tests.rs`. INFERRED unaffected, because the inline-dispatch path does not go through `process_subscriber_for_batch`. Stated as inference, not fact.

Known load-sensitive, flag to whoever reviews the fix: `test_in_flight_transaction_gap_is_held` (a bare 4s sleep that can pass **vacuously** under load, plus the suite's longest transaction hold and so its largest contribution to `xmin` pinning) and `test_rolled_back_gap_fence_clears_without_record` (its 6s sleep must cover two ticks *plus* instance-wide `xmin` advancing; a failure there after this change is more likely a concurrent binary than the fix).

## 8. Phasing

Each phase is independently committable and leaves the suite green. Positive controls land in P2, i.e. no later than the phase that changes flushing behaviour.

- **P1 — split the two jobs (no behaviour change yet).** Add `PendingCheckpoint::record_processed` and the publishable predicate plus their unit tests. Nothing calls `record_processed` yet, so behaviour is unchanged and the suite stays green. `fix(pg): separate checkpoint accounting from the published position`.
- **P2 — harden the existing controls.** Convert `test_batched_checkpoint_flushes_at_batch_size` / `_at_max_delay` to isolated-table exact-value assertions, and add the two positive controls of §7.3. These pass pre-fix, which is the point: they establish the "does not stall, cadence still works" baseline the next phase must not break. Add the integration-test copy of `cu_isolated_events_table` and its `setup_trigger`-before-`start_listener` wiring here, since P3/P4 both need it. `test(pg): exact-value and no-stall controls for live checkpointing`.
- **P3 — the fix.** Q1's `SubscriberState.contiguous_event_id` + seeding `SELECT`; both per-event sites switch to `record_processed` with eager seeding; the contiguous branch switches to `update()`; move `try_flush_pending_checkpoint` out of the advance branch under `!replay_always`; `debug_assert`; publishable predicate applied at all three flush sites. `fix(pg): publish only contiguous checkpoints from the live listener path`.
- **P4 — regression tests.** The three §7.2 tests plus the `ReplayAlways` unit guard. Land after P3 so they are green on commit; verify they fail against P3 reverted (§9.2). `test(pg): pin the live path against publishing above a held hole`.
- **P5 — hygiene and docs.** CHANGELOG `### Fixed` entry per §6; rustdoc on `record_processed`, the new `SubscriberState` field, and `new_with_event_id`; update `flush_checkpoint`'s hazard rustdoc to cite spec 0027 R1 alongside 0026 R1/R2; mark this spec Implemented with a §10 summary in 0026's style. `docs(pg): record the live-path contiguous checkpoint fix`.

## Phases (JSON)

```json
[
  {
    "id": "P1",
    "name": "Separate checkpoint accounting from the published position",
    "difficulty": "standard",
    "requirements": ["R3", "R4"],
    "summary": "Add PendingCheckpoint::record_processed (bumps events_since_checkpoint only) and a shared publishable predicate (global_sequence > 0) in epoch_pg/src/event_bus/checkpoint.rs, with unit tests. No call sites change; behaviour is unchanged.",
    "commit": "fix(pg): separate checkpoint accounting from the published position"
  },
  {
    "id": "P2",
    "name": "Exact-value and no-stall positive controls",
    "difficulty": "hard",
    "requirements": ["R2", "R3", "R4"],
    "summary": "Add an integration-test copy of cu_isolated_events_table (LIKE + own sequence, setup_trigger before start_listener, Uuid-unique channel). Convert test_batched_checkpoint_flushes_at_batch_size and _at_max_delay to exact-value assertions on that table. Add test_backstop_hole_still_advances_checkpoint (snapshot_fencing false, gap_timeout 500ms, zero load sensitivity, mandatory) and test_fence_cleared_hole_still_advances_checkpoint (snapshot_fencing true, gap_timeout 30s, no gap-timeout row; load-sensitive by design and must fail rather than report inconclusive). These pass pre-fix.",
    "commit": "test(pg): exact-value and no-stall controls for live checkpointing"
  },
  {
    "id": "P3",
    "name": "Publish only contiguous checkpoints from the live path",
    "difficulty": "hard",
    "requirements": ["R1", "R3", "R4", "R5", "R6", "R7"],
    "summary": "In epoch_pg/src/event_bus: add SubscriberState.contiguous_event_id plus SubscriberState::new_with_event_id, and add last_event_id to the start_listener checkpoint-seeding SELECT. In process_subscriber_for_batch, both per-event sites call record_processed and seed the PendingCheckpoint from state.contiguous_checkpoint and its paired id; the contiguous branch switches from direct field assignment to update(); move the try_flush_pending_checkpoint call out of the `new_contiguous > contiguous_before` branch so it runs unconditionally under a !replay_always guard, setting cached_checkpoint only from a flush that actually happened; add debug_assert!(pending <= state.contiguous_checkpoint) before the flush. Apply the publishable predicate in try_flush_pending_checkpoint, flush_expired_checkpoints and flush_all_pending_checkpoints. Do not merge the two advancers. Do not touch advance_catchup_prefix or the public update_checkpoint.",
    "commit": "fix(pg): publish only contiguous checkpoints from the live listener path"
  },
  {
    "id": "P4",
    "name": "Regression tests for the live path",
    "difficulty": "hard",
    "requirements": ["R1", "R5", "R6"],
    "summary": "Add test_live_shutdown_does_not_publish_above_held_hole (primary; default Synchronous, commented as such), test_live_batched_flush_does_not_publish_above_held_hole (Batched batch_size 1000 / max_delay_ms 300; one unavoidable fixed sleep >= 2x the hard-coded 1s flush_interval, taken after delivery is verified, with a comment forbidding conversion to a poll loop), and test_live_deser_skip_above_hole_does_not_publish_above_hole (raw inserts with garbage data, WARN asserted via captured_logs_contain_since). All on an isolated table, gap_timeout 30s, exact-equality assertion against a committed below-hole sequence, and observe-then-release-then-assert ordering. Plus a ReplayAlways unit guard. Verify each fails with P3 stashed.",
    "commit": "test(pg): pin the live path against publishing above a held hole"
  },
  {
    "id": "P5",
    "name": "CHANGELOG, rustdoc, spec status",
    "difficulty": "standard",
    "requirements": ["R7"],
    "summary": "CHANGELOG ### Fixed entry mirroring spec 0026's R5 entry: the silent-loss fix plus the user-visible readiness delay bounded by gap_timeout. Rustdoc for record_processed, the new SubscriberState field and new_with_event_id. Extend flush_checkpoint's hazard rustdoc to cite spec 0027 R1. Mark spec 0027 Implemented with an Implementation Summary in 0026's style.",
    "commit": "docs(pg): record the live-path contiguous checkpoint fix"
  }
]
```

## 9. Acceptance criteria

1. New tests pass; existing gap-fence and gap-timeout tests pass, modulo the two deliberate hardenings in P2.
2. Reverting P3 makes §7.2 tests 1–3 fail — verified **mechanically by stashing**, not by inspection.
3. `cargo fmt --check` clean, and `cargo clippy --all-targets -p epoch_pg -p epoch_core -- -D warnings` clean. **Do not run workspace-wide clippy as the gate**: there is a pre-existing `ambiguous_glob_reexports` failure at `epoch/src/lib.rs` which is out of scope and must not be fixed here.
4. Event-bus binary passes in isolation and under a concurrent writer on `epoch_events`, on consecutive runs. A green `--workspace` pair is a weak gate: shared DB, load-sensitive tests.
5. No `unwrap()`/`expect()` added in library code (tests exempt). New/changed `pub(crate)` items carry rustdoc.
6. `git diff --stat` shows no migration file and no change under any `pub` signature (R7).

## 10. Out of scope

- Merging `advance_catchup_prefix` with `advance_contiguous_checkpoint` (§2; already rejected).
- A monotonic `WHERE` guard on `flush_checkpoint` (§4 Q3; breaks deliberate rewind via `update_checkpoint`).
- Suppressing redundant idempotent writes (§4 Q2).
- Making `flush_interval` configurable (§7.2; public-behaviour change).
- CLOUD-227 (`ReplayAlways` HWM loss window), CLOUD-231 (duplicate-subscriber first-wins in the priority-group dedup via `seen_sids`).
- Testing the reconnect flush trigger (§7.4).

## 11. Residual uncertainty

Stated as uncertainty rather than papered over:

- **Lost-outcome direction.** Task inputs are built by draining `subscriber_states.remove(sid)` and `pending_checkpoints.remove(sid)` per subscriber, deduplicated through `seen_sids` (~1558). Because the pending value is *removed* from the map and handed to the task, an outcome that never returns loses the in-memory pending checkpoint. INFERRED to fail **safe** (a lost pending means a checkpoint is not advanced, rather than advanced too far), and the code reads that way, but this was not proven by test. If a reviewer wants it proven, it is a separate exercise.
- **`inline_dispatch_integration_tests.rs` / `transaction_integration_tests.rs`** are INFERRED unaffected (§7.5), not verified.
- **The fence-cleared control's flakiness rate under real CI load is unknown.** The decision to let it fail hard (§7.3) is deliberate but may generate noise; the backstop control is the load-insensitive fallback if it has to be quarantined later.
