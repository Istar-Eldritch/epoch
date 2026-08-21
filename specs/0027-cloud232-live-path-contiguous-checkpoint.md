# Spec 0027: Live-Path Contiguous Checkpoint

**Issue:** CLOUD-232 · **Status:** Reviewed, ready to implement · **Crate:** `epoch_pg`
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

- The ticket warns that publishing only the contiguous value can **permanently stall** a subscriber on a rolled-back transaction. It cannot, but the reason is subtler than "the skip logic lives inside `advance_contiguous_checkpoint`" — that alone only proves the skip exists, not that it is ever reached again. The full argument: `advance_contiguous_checkpoint` resolves a gap only when *called*, and it is called only from the inner batch loop, which needs non-empty `rows`. The saving grace is `WakeReason::TimerTick` (`flush_interval`, a hard-coded 1s) re-entering the drain unconditionally, combined with the batch query being `global_sequence > min_checkpoint`, which stays non-empty for as long as any event sits above the hole. So the backstop fires even on an otherwise idle bus. And if there is *nothing* above the hole, `has_events_above` is false, no gap is recorded, and holding at the seed is correct rather than a stall.
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

**Why eager seeding rather than `Option<u64>` for the position.** There is always a legitimately publishable contiguous value: the current one, normally equal to what is already persisted. That keeps the field non-optional, needs no `None`-skipping in any flush site, and preserves `max_delay_ms` measuring from the first unflushed event rather than restarting at the first advance. An earlier `Option<u64>` proposal was dropped for this reason.

**The seed is not itself publishable.** A pending that still sits where it was seeded carries no new information: writing it would re-write the persisted row with an identical sequence. §4 Q1 therefore makes the publishable predicate "has advanced beyond its seed", not "is non-zero". Consequence to accept: while a hole is held, the pending is never published, so `events_since_checkpoint` keeps accumulating and `first_event_time` keeps ageing until the prefix moves. That is bounded by `gap_timeout` and is the correct behaviour — the counter is measuring unflushed work, and none of it has been flushed.

### 3.3 The invariant, which is the spine of §5

> **After a flush from any site, the persisted checkpoint MAY lag `state.contiguous_checkpoint`; it MUST NEVER lead it.**

Lagging is legitimate and expected: between flushes, and under `Batched` by design. Leading is the bug. Every requirement below is a consequence of this asymmetry, and the tests in §7 pin both directions of it — the regression tests pin "never leads", the positive controls pin "the lag is bounded and does eventually close".

### 3.4 Call-site placement details the implementer will hit

- `try_flush_pending_checkpoint` uses `take_if(|p| should_flush_checkpoint(p, mode))`, so a call that does not meet the threshold leaves `pending_checkpoint` intact. Moving the call out of the branch is therefore safe with respect to the carried-across-batches contract.
- `cached_checkpoint` is currently set inside the same branch (`Some(new_contiguous)`, then overwritten from `local_cache`) and is merged into the listener cache by the caller (~1602). **Set it only from `local_cache`, i.e. only when a flush actually happened.** The existing unconditional `cached_checkpoint = Some(new_contiguous)` on advance is **deleted**, not kept: under `Batched` a prefix advance that does not trip the threshold writes nothing, so keeping that line would cache a value that was never persisted. This is a real behaviour change and is stated here so it is not mistaken for a refactor.
  Mitigating fact, so nobody over-weights this: the listener's `checkpoint_cache` currently has **no readers** — every occurrence is an insert or a pass-by-mut, and nothing branches on it. So the correction is about not encoding a false invariant, not about a live bug.
- `process_subscriber_for_batch` nulls both `pending_checkpoint` and `cached_checkpoint` for a `ReplayAlways` subscriber *after* the branch. The new unconditional flush must sit inside a `!replay_always` guard, not merely rely on that later nulling, or a `ReplayAlways` subscriber would write a checkpoint row before it is cleared.
- **Where the publishable predicate goes, and the destructive misreading to avoid.** In `try_flush_pending_checkpoint` the pending is consumed by `take_if(|p| should_flush_checkpoint(p, mode))`. Fold the predicate **into that closure** — `take_if(|p| p.is_publishable() && should_flush_checkpoint(p, mode))`. Applying it as an early return *after* the `take_if` silently destroys the pending, which has already been moved out of the `Option`. The same trap exists in `flush_expired_checkpoints` and `flush_all_pending_checkpoints`, which both `remove` the entry from the map before flushing: the predicate must go in the selection (`filter` / key choice), or a rejected entry is dropped along with its `first_event_time` and `events_since_checkpoint`, which is direct R4 damage. "Apply it at all three sites" is not sufficient instruction; this bullet is.
- **The `None` arm of the contiguous branch survives.** The branch is `match pending { Some(p) => …, None => PendingCheckpoint::new(new_contiguous, checkpoint_event_id) }`. That arm stays, and is the publishable constructor of §4 Q1. It is genuinely reachable after this change: a batch in which every row hits the `state.processed_ahead` early-continue seeds no pending, and the gap is then closed by `advance_contiguous_checkpoint`. Dropping it would lose the advance entirely under `Batched`.

## 4. Settling the brief's open questions

The brief left four open. All four are decided here, and **all four are now signed off**. Q1 was escalated to a human, approved, and then amended in review — the amendment (a seed-based publishable predicate replacing the original `> 0` rule) is also signed off. Read Q1 in full rather than skimming the decision line; the amendment changes the shape of the code.

### Q1 — where the seed's paired `event_id` comes from. **DECISION: add the field. SIGNED OFF, with the amendment below.**

Verified current shape: `SubscriberState` (`subscriber_state.rs`) has exactly three fields — `contiguous_checkpoint: u64`, `processed_ahead: HashSet<u64>`, `gap_first_seen: HashMap<u64, GapObservation>` — and **no paired event id**. The seeding query in `start_listener` selects **only** `last_global_sequence`. `epoch_event_bus_checkpoints.last_event_id` is nullable `UUID` (`m003_create_event_bus_infrastructure.rs`, ~66).

Spec 0026 **R4** requires `last_event_id` to match `last_global_sequence`. Eager seeding therefore needs an id, and the alternatives are worse:

- `seq_to_id.get(&contiguous_before)` does not merely risk a miss, it is **structurally guaranteed** to miss for the subscriber that sets the minimum: the batch query is `global_sequence > min_checkpoint`, so the row *at* that subscriber's contiguous position is never in the batch. For a single-subscriber bus that is always the case. Silently violates R4.
- The listener's existing `last_event_ids` map is **not** a substitute either, and is ruled out here by name because an implementer will find it and assume otherwise. Its comment claims it holds "the event UUID associated with the most recently flushed contiguous checkpoint", which is false: it is written from `outcome.last_event_id`, which is set from ahead-of-hole events too. It is also empty on boot, so it cannot seed a restarted listener at all.
- Seeding `Uuid::nil()` and hoping a flush never happens before the first advance is exactly the case the `Batched` threshold makes reachable. Violates R4 in the field.

So: add `contiguous_event_id: Uuid` to `SubscriberState`, and add `last_event_id` to the seeding `SELECT` (now `query_as::<_, (i64, Option<Uuid>)>`, mapping `None` to `Uuid::nil()`). Keep `SubscriberState::new(checkpoint)` as-is (21 call sites, mostly unit tests in `subscriber_state.rs`) seeding `contiguous_event_id: Uuid::nil()`, and add `SubscriberState::new_with_event_id(checkpoint, event_id)` for the production seeding site. `advance_contiguous_checkpoint` does not need to maintain the field; `process_subscriber_for_batch` sets it alongside `checkpoint_event_id` when the prefix advances.

**The publishable predicate, amended after review. This supersedes the earlier `global_sequence > 0` rule.**

The original rule was aimed at one case: a brand-new subscriber seeds `(0, nil)`, and writing that creates a sequence-0 row with a nil `last_event_id` corresponding to nothing. Harmless (a 0 row and a missing row both mean "replay from the start") but meaningless, and it makes R5 formally false.

Review surfaced a second case the `> 0` rule does not cover: an *existing* subscriber seeded from a row whose `last_event_id` is NULL (written by the public `update_checkpoint`, or a legacy row) would map `None → nil` and, because the seed is flushable under `> 0`, re-write that row degrading NULL to nil. Note the narrower form of this hazard, since the review overstated it: a row with a *real* id reads back as `Some(id)` and cannot be clobbered. The genuine cost is a redundant write that replaces "unknown" with a nil UUID that is affirmatively wrong.

Both cases share a cause: **the seed is being published even though it carries no new information.** So the predicate becomes:

> Add `seed_sequence: u64` to `PendingCheckpoint`. A pending is publishable only when `global_sequence > seed_sequence`.

This is strictly simpler than the rule it replaces and subsumes it. A fresh subscriber seeds at 0 and stays unpublished until it genuinely advances, so no sequence-0 row is ever written. An existing subscriber never re-writes its own seed, so NULL is never degraded to nil, and `contiguous_event_id` can stay a plain `Uuid` because nil is unpublishable by construction. It also removes the redundant-write half of Q2 for free, and needs no external state: the predicate is answerable from `PendingCheckpoint` alone, so all three flush sites get it without plumbing.

**The one trap, and it is the whole design.** There are two construction paths and they must set `seed_sequence` differently:

- the **eager seed** (created from `state.contiguous_checkpoint` when a batch first touches a subscriber) sets `seed_sequence` to its own `global_sequence`, so it is born unpublishable;
- the **advance** path (`PendingCheckpoint::new` in the `None` arm of the contiguous branch, created *because* the prefix moved) must set `seed_sequence` **below** its position, so it is born publishable.

Get these backwards and either the fix does nothing (seed always published) or the checkpoint never advances at all (advance never published). Use two distinct named constructors rather than a bool parameter, and unit-test both.

**Retained decision:** add `contiguous_event_id: Uuid` to `SubscriberState`, and add `last_event_id` to the seeding `SELECT` (now `query_as::<_, (i64, Option<Uuid>)>`, mapping `None` to `Uuid::nil()`). Keep `SubscriberState::new(checkpoint)` as-is (21 call sites, mostly unit tests in `subscriber_state.rs`) seeding `contiguous_event_id: Uuid::nil()`, and add `SubscriberState::new_with_event_id(checkpoint, event_id)` for the production seeding site. `advance_contiguous_checkpoint` does not need to maintain the field; `process_subscriber_for_batch` sets it alongside `checkpoint_event_id` when the prefix advances.

### Q2 — suppress redundant idempotent writes? **DECISION: out of scope.** Safe to decide

**Partly resolved by Q1's amendment**: the seed-based predicate already suppresses the redundant write that motivated this question, since a pending that has not advanced is never published. What remains out of scope is the broader idea of skipping a flush whose position merely equals the last *cached* persisted value.

`try_flush_pending_checkpoint` could skip when the pending position equals the cached persisted value, using the existing `checkpoint_cache`. Reasons to leave it: the write is a single indexed upsert on a tiny table; and it interacts with `cached_checkpoint` merge semantics (§3.4) in a way that adds review surface to a P1 correctness fix. Correcting the original cost estimate, which was wrong: under the default `Synchronous` mode `should_flush_checkpoint` always returns true, so the moved unconditional flush would have written on **every batch processing any event above a hole**, not "at most once per `Batched` threshold trip". Still bounded (events already in `processed_ahead` `continue` before `record_processed`, so an idle tick over a held hole produces no pending and no write), but the stated bound was not the real one. Note also that `process_subscriber_for_batch` deliberately uses a *fresh local cache*, not the listener-level one, so the comparison would be against an empty map there and would not fire anyway without further plumbing. File as a follow-up if the redundant writes ever show up in `pg_stat_statements`.

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
- **R5.** A persisted checkpoint's `last_event_id` MUST be **no worse paired** than the existing advance path already achieves (spec 0026 R4), and a never-advanced seed MUST NOT be published at all (§4 Q1). Stated this way deliberately: the existing `checkpoint_event_id` fallback (`seq_to_id.get(new_contiguous).or(last_event_id).unwrap_or(nil)`) **already** publishes a mismatched id when an advance skips a hole, because the skipped sequence has no row. Fixing that fallback is not in this spec's scope, so R5 must not be phrased as an absolute match it cannot deliver. What this spec does guarantee: the seed never introduces a *new* mismatch, because it is never published.
- **R6.** `ReplayAlways` subscribers MUST continue to write no checkpoint from this path.
- **R7.** No public API change, no error-type change, no schema change, no migration (spec 0026 R7). `PendingCheckpoint`, `SubscriberState` and `process_subscriber_for_batch` are all `pub(crate)` or private; `CheckpointMode`, `should_flush_checkpoint` and the checkpoints table are untouched. **If the implementer concludes any of these must change, stop and raise it before writing code** — it invalidates the scope of this spec.

## 6. Behavioural risk this change owns

This is a first-class consequence, not a footnote.

**Scoped correctly after review. An earlier draft of this section overstated the blast radius and that overstatement must not survive into the CHANGELOG.**

The claim to retract: "previously the bound was effectively the flush interval, because the checkpoint jumped the hole". That is **false for the default mode**. The only live-path `try_flush_pending_checkpoint` call sits inside the advance branch, and `flush_expired_checkpoints` early-returns for `Synchronous`. So under `Synchronous` the persisted checkpoint **already** never jumps a held hole in steady state — it moves only when the contiguous prefix moves. The existing `test_in_flight_transaction_gap_is_held` asserts exactly that and passes today.

What is actually true:

- Under **`Synchronous` (the default)**, this fix changes behaviour only at the **shutdown and reconnect boundary**. Readiness in steady state is unaffected, because the persisted value already held at the hole.
- Under **`Batched`**, the change does reach steady state: the timer-tick flush used to publish above a hole, so a subscriber that used to report caught-up will now report not-caught-up while the hole is open, **bounded by `gap_timeout`** (default 5s), after which the backstop skips it.

Since readiness reads the persisted value (`position_for_mode` → `get_checkpoint`), the readiness change follows the same split.

Consequences:

- The **CHANGELOG entry** under `### Fixed` states two facts: silent loss on shutdown/reconnect is closed in **all** modes, and on the `Batched` timer tick. The `gap_timeout`-bounded readiness delay is attached **only to `Batched`**. Do not describe a steady-state change on the default configuration; overstating severity trains readers to discount the project's severity claims.
- No existing test's `wait_until_caught_up` bound is newly breached. Note this conclusion is right for a different reason than an earlier draft gave: the suite's bounds are **not** uniformly generous (they run 100ms, 200ms, 500ms, 800ms, 3s, with a single 15s in a CLOUD-226 test), but they are all default-`Synchronous` and therefore unaffected per the split above. The stated reason matters more than the conclusion, because the reason is what the next change will be checked against.

## 7. Test plan

Priorities are the brief's §8 and are deliberately counterintuitive. Do not reorder them.

### 7.1 Two decisions that shape everything else

**Use a private events table per test.** `ReliableDeliveryConfig::events_table` is public and `setup_trigger` (`mod.rs`, ~933) supports a custom table. The unit-test helper `cu_isolated_events_table` (`mod.rs`, `#[cfg(test)]`, ~3717) does `CREATE TABLE … (LIKE epoch_events INCLUDING ALL)` plus its **own sequence**; the integration binary needs its own copy, since that helper is not reachable from `tests/`. `LIKE` does **not** copy triggers, so `setup_trigger()` must run before `start_listener()`, with a `Uuid`-unique channel name.

This buys three things: the sequence is no longer shared, so **exact-equality** assertions become possible; `bus_name` is `config.events_table`, so checkpoint and gap-timeout rows are per-test; and no `truncate_epoch_tables` call is needed, so these tests contribute nothing to the `ACCESS EXCLUSIVE` contention that previously wedged this project for five hours.

**It does not buy immunity from sibling binaries, and this spec must not claim it does.** The checkpoint and gap-timeout rows live in the shared `epoch_event_bus_checkpoints` / `_gap_timeouts` tables, so a sibling binary's `truncate_epoch_tables` (which does `TRUNCATE … RESTART IDENTITY`) can still wipe an isolated test's checkpoint row mid-flight. Exact-equality assertions do not prevent that; what they do is convert it from a **silent pass into a loud failure**, which is the better trade and the actual justification. Related known gap: nothing deletes those rows afterwards, so they accumulate per run, keyed by the random `bus_name`. Left as-is deliberately — they are tiny and a cleanup pass is more shared-table contention.

**Assert exact equality, never `< seq_hole`.** The existing near-miss pattern `checkpoint.is_none() || matches!(checkpoint, Some(s) if s < seq_n)` **passes when nothing was ever written**, including when a sibling binary's `TRUNCATE` wipes the row mid-test. Committing an event *below* the hole first gives a known-good value to assert equality against, which simultaneously proves the leak is closed, proves the write path is alive, and proves the fix did not move the checkpoint backwards. **This is a review gate for this spec.**

**Ordering discipline, inherited from CLOUD-226 and mandatory:** capture the observation, then roll back or commit the held transaction and shut down, **then** assert. An `assert!` that unwinds with the transaction still open is exactly the shape that produced the wedge. Test pools set `lock_timeout = '30s'` and `idle_in_transaction_session_timeout = '60s'`, so any test holding a hole must stay well inside 60s.

### 7.2 Regression tests (R1)

All share: isolated table; `gap_timeout: 30s` so the backstop cannot fire inside the window; a committed event *below* the hole; `claim_hole_uncommitted` (`pgeventbus_integration_tests.rs`, ~5605) to open the hole; two committed events above it; and a **bounded poll** proving the above-hole events were delivered, which is what proves `pending_checkpoint` was populated with max-seen in the first place. (Test 3 substitutes a different signal for that poll — see below.)

> **BLOCKER-GRADE PREREQUISITE, do not skip.** The helpers named here are hardcoded to the shared table and **cannot be pointed at an isolated one as written**:
> - `claim_hole_uncommitted` and `insert_committed_event` both embed `INSERT INTO epoch_events` literally and take no table argument. Used unchanged, the hole is burned on the **shared** sequence and never appears in the isolated table, so the listener sees no gap, and **all three regression tests pass vacuously pre-fix**.
> - `PgEventStore::new` hardcodes `events_table: "epoch_events"`. A bus pointed at an isolated table therefore receives no events at all and the test hangs to timeout. Use `PgEventStore::with_table` (async; it also ensures the `txid` column so fencing works), or raw inserts in the style of the `cu_*` helpers.
>
> So P2/P4 must add a `table: &str` parameter to both helpers, and the isolated table's hole must be claimed through **that table's** `DEFAULT nextval` so it burns the private sequence. This is real work, not a detail, and §9.2's "verify it fails with P3 stashed" is the only gate that would catch it being skipped.

1. **`test_live_shutdown_does_not_publish_above_held_hole` — PRIMARY.** Default `Synchronous`, stated explicitly in a comment, since the whole point is that only the shutdown flush can leak on the production default. `shutdown()` with the transaction still open, then read. Asserts the persisted checkpoint **equals** `seq_below`. Pre-fix this reads `Some(seq_above2)`. **Also assert the persisted `last_event_id` equals the below-hole event's id** — this is the only DB-level check of R5 anywhere in the plan, and the seed path is precisely where the pairing can break.
2. **`test_live_batched_flush_does_not_publish_above_held_hole`.** Targets `flush_expired_checkpoints` with `Batched { batch_size: 1000, max_delay_ms: 300 }` — `batch_size` high so only the delay can fire.
3. **`test_live_deser_skip_above_hole_does_not_publish_above_hole`.** The deserialization-skip branch is the **second** unconditional write site and no existing test covers it above a hole. Write the above-hole events as raw inserts with garbage `data` so deserialization fails, and assert via `common::captured_logs_contain_since` that the WARN actually fired — otherwise the test is vacuous. Two things this test must state explicitly, because the shared preamble does not apply to it: **the delivery poll is impossible here** (the deser branch `continue`s, so those events are never delivered) and the WARN assertion is its *substitute*, not an addition; and it must **name its `CheckpointMode` and its flush trigger** (default `Synchronous` plus `shutdown()`, or `Batched` plus the tick). Without the latter an implementer can write a test that never flushes at all and passes vacuously in both directions.

**One fixed sleep is unavoidable in test 2, and a reviewer must not "clean it up" into a poll loop.** "Checkpoint has not advanced" is both the expected post-condition and what a stalled listener looks like, so there is no positive edge to poll for; a poll loop here cannot fail. Mitigate by verifying delivery *before* the sleep starts, so the sleep only has to cover the flush tick, and size it at `>= 2x flush_interval`. Note `flush_interval` is a hard-coded `Duration::from_secs(1)` in `start_listener` (~1237), so every timer-tick test is floored at ~1s; making it configurable is a public-behaviour change and out of scope.

### 7.3 Positive controls: the fix must not stall a subscriber

These must land no later than the phase that changes flushing behaviour (§8), so a stalling regression cannot pass unnoticed.

- **`test_backstop_hole_still_advances_checkpoint` — MANDATORY, and the one to trust.** With `snapshot_fencing: false` and `gap_timeout: 500ms` it depends only on elapsed wall-clock, so it has **zero load sensitivity**. Asserts the checkpoint reaches `>= seq_above` (`>=`, since after a backstop skip the prefix legitimately runs to head) and that a gap-timeout row exists. (R2)
- **`test_fence_cleared_hole_still_advances_checkpoint` — lands `#[ignore]`d. DECISION CHANGED after review.** Same shape with `snapshot_fencing: true`, `gap_timeout: 30s` so a pass cannot be the backstop in disguise, asserting **no** gap-timeout row. **Load-sensitive, and an isolated table does not help**, because clearing needs `snap.xmin >= fence_xmax` and `xmin` is **instance-wide**: any transaction open anywhere on the server pins it, and sibling tests in this very suite hold transactions for 4s+. Also, `batch_snapshot` is only queried once a gap is already active, so `fence_xmax` is `None` on first observation and backfilled on the next batch, giving a two-tick (~2s) floor.
  An earlier draft had it fail hard on the argument that an inconclusive control is useless. That argument loses to a simpler one: this project has already lost hours to flaky tests, and the coverage is **redundant** — the backstop control above already proves "no permanent stall" with zero load sensitivity, and the existing `test_rolled_back_gap_fence_clears_without_record` already covers the fence route. A third, flaky copy of assurance you hold twice is a net negative. It therefore ships `#[ignore]`d, with a doc comment stating what it proves and to run it on a quiet instance. Accept the known cost: ignored tests rot, and this one will eventually stop compiling if nobody runs it. (R2)
- **Harden `test_batched_checkpoint_flushes_at_batch_size` and `test_batched_checkpoint_flushes_at_max_delay`** from `assert!(checkpoint.is_some())` to exact-value assertions on an isolated table. This is the control that catches the §3.1 counter regression, and it converts two currently decorative tests into real ones. Budget the rewrite honestly: both drive events through `event_store.store_event`, so per the §7.2 prerequisite they must move to `PgEventStore::with_table` or raw inserts, and drop `truncate_epoch_tables`. (R3, R4)
- **Harden `test_synchronous_checkpoint_still_works` the same way. ADDED after review, and it closes the most dangerous gap in this plan.** Every other positive control here is `Batched`-only, yet the change most likely to break cadence — moving the flush call out of the advance branch — affects `Synchronous` too, and `Synchronous` is the default. Without this there is **no control that the default mode still advances its checkpoint per batch**. Exact-value, isolated table. (R3)
- **Unit tests in `checkpoint.rs`:** `record_processed` bumps the counter and leaves `global_sequence`/`event_id`/`first_event_time` untouched; `update` still does both jobs; and, per §4 Q1, **both** constructors are pinned — an eagerly-seeded pending is **not** publishable, and one created by the advance path **is**. That pair is the trap the whole design turns on, so it is unit-tested rather than left to review. No database, and they will still mean something in five years. (R3, R5)
- **Unit test in `subscriber_state.rs` or `mod.rs`:** `ReplayAlways` regression guard — no checkpoint written from the live path. (R6)

### 7.4 Reconnect is a knowingly-accepted gap

The reconnect flush is reached only from `WakeReason::Notification(Err(e))`, which needs the listener's `PgListener` connection killed externally via `pg_terminate_backend`. That is racy to target and is a cross-connection kill on a shared instance. It is also redundant: it is the **same function with the same missing guard** as the shutdown path, so test 1 covers the defect and only the trigger differs. Recorded here deliberately, not overlooked.

### 7.5 Existing tests

**No existing test encodes the bug as expected**, verified across `pgeventbus_integration_tests.rs` and `saga_adapter_integration_tests.rs`. There are **about a dozen** bare `assert!(checkpoint.is_some())` sites, not the four an earlier draft claimed; the characterisation (insensitive in both directions) holds, the count did not. The two lower-bound assertions (`test_in_flight_transaction_gap_is_held` post-commit, `test_rolled_back_gap_fence_clears_without_record`) are cases where the prefix legitimately reaches the asserted value, and the second doubles as an existing positive control — which is part of why §7.3 quarantines the new fence-cleared test rather than shipping it flaky.

Not audited: `inline_dispatch_integration_tests.rs`, `transaction_integration_tests.rs`. INFERRED unaffected, because the inline-dispatch path does not go through `process_subscriber_for_batch`. Stated as inference, not fact.

Known load-sensitive, flag to whoever reviews the fix: `test_in_flight_transaction_gap_is_held` (a bare 4s sleep that can pass **vacuously** under load, plus the suite's longest transaction hold and so its largest contribution to `xmin` pinning) and `test_rolled_back_gap_fence_clears_without_record` (its 6s sleep must cover two ticks *plus* instance-wide `xmin` advancing; a failure there after this change is more likely a concurrent binary than the fix).

## 8. Phasing

Each phase is independently committable and leaves the suite green. Positive controls land in P2, i.e. no later than the phase that changes flushing behaviour.

- **P1 — split the two jobs (no behaviour change yet).** Add `PendingCheckpoint::record_processed` and the publishable predicate plus their unit tests. Nothing calls `record_processed` yet, so behaviour is unchanged and the suite stays green. `fix(pg): separate checkpoint accounting from the published position`.
- **P2 — harden the existing controls.** Convert `test_batched_checkpoint_flushes_at_batch_size` / `_at_max_delay` / `test_synchronous_checkpoint_still_works` to isolated-table exact-value assertions, and add the two positive controls of §7.3. Add the integration-test copy of `cu_isolated_events_table` with its `setup_trigger`-before-`start_listener` wiring, **the `table` parameter on `claim_hole_uncommitted` and `insert_committed_event`, and the `PgEventStore::with_table` conversion** (§7.2 prerequisite) here, since P3/P4 both need all of it. `test(pg): exact-value and no-stall controls for live checkpointing`.
  **What P2 does and does not prove.** An earlier draft claimed these "establish the baseline the next phase must not break". Overstated: pre-fix, `test_backstop_hole_still_advances_checkpoint`'s sequence assertion is satisfied **by the bug itself**, since max-seen already leaks a value at or above the target. Pre-fix these are trivially green and their real value is as a **post-P3 tripwire**: once the fix lands, a version that stops the leak by never advancing turns them red. The ordering is still right for exactly the stated reason — the tripwire must already exist when the behaviour changes — but do not mistake a pre-fix pass for evidence of anything.
- **P3 — the fix.** Q1's `SubscriberState.contiguous_event_id` + seeding `SELECT`; both per-event sites switch to `record_processed` with eager seeding; the contiguous branch switches to `update()`; move `try_flush_pending_checkpoint` out of the advance branch under `!replay_always`; `debug_assert`; publishable predicate applied at all three flush sites. `fix(pg): publish only contiguous checkpoints from the live listener path`.
- **P4 — regression tests.** The three §7.2 tests plus the `ReplayAlways` unit guard. Land after P3 so they are green on commit; verify they fail against P3 reverted (§9.2). `test(pg): pin the live path against publishing above a held hole`.
- **P5 — hygiene and docs.** CHANGELOG `### Fixed` entry per §6, **scoped as §6 specifies** (do not claim a steady-state change on the default mode); rustdoc on `record_processed`, the new `SubscriberState` field, `new_with_event_id`, **both `PendingCheckpoint` constructors and the publishable predicate** (all new `pub(crate)` items, which acceptance criterion 5 requires); update `flush_checkpoint`'s hazard rustdoc to cite spec 0027 R1 alongside 0026 R1/R2; mark this spec Implemented with a §10 summary in 0026's style. `docs(pg): record the live-path contiguous checkpoint fix`.

## Phases (JSON)

> Shape note: `implement-pipeline.ts` requires a top-level **object** with a `phases` array, and each entry needs an integer `phase` and a non-empty string `focus`. A bare array, or `id`/`name` in place of `phase`/`focus`, parses to **zero phases silently** — the pipeline then runs nothing rather than erroring. Extra keys are ignored. Only the exact string `"hard"` routes to the stronger model.

```json
{
  "spec": "specs/0027-cloud232-live-path-contiguous-checkpoint.md",
  "depends_on_specs": ["specs/0026-cloud226-contiguous-catchup-checkpoint.md"],
  "phases": [
    {
      "phase": 1,
      "focus": "epoch_pg/src/event_bus/checkpoint.rs: split checkpoint accounting from the published position. Add PendingCheckpoint::record_processed (bumps events_since_checkpoint ONLY, leaving global_sequence/event_id/first_event_time untouched); add a seed_sequence field plus TWO distinct named constructors -- an eager-seed constructor that sets seed_sequence to its own global_sequence (born UNpublishable) and an advance constructor that sets it below (born publishable); add the publishable predicate global_sequence > seed_sequence. Unit-test both constructors and record_processed. No call sites change, behaviour unchanged.",
      "effort": "S",
      "difficulty": "standard",
      "requirements": ["R3", "R4", "R5"],
      "depends_on": [],
      "commit": "fix(pg): separate checkpoint accounting from the published position"
    },
    {
      "phase": 2,
      "focus": "epoch_pg/tests: test infrastructure and positive controls, all passing pre-fix. Add an integration-test copy of cu_isolated_events_table (CREATE TABLE LIKE + own sequence, setup_trigger BEFORE start_listener, Uuid-unique channel). PREREQUISITE, do not skip: add a table parameter to claim_hole_uncommitted and insert_committed_event (both currently hardcode INSERT INTO epoch_events, so on an isolated table the hole is burned on the WRONG sequence and every regression test passes vacuously), and convert event writes to PgEventStore::with_table or raw inserts (PgEventStore::new hardcodes epoch_events, so the bus would receive nothing and hang). Convert test_batched_checkpoint_flushes_at_batch_size, _at_max_delay and test_synchronous_checkpoint_still_works to exact-value assertions on the isolated table. Add test_backstop_hole_still_advances_checkpoint (snapshot_fencing false, gap_timeout 500ms, zero load sensitivity, MANDATORY). Add test_fence_cleared_hole_still_advances_checkpoint as #[ignore] with a doc comment (instance-wide xmin makes it flaky; the backstop control and the existing test_rolled_back_gap_fence_clears_without_record already cover this).",
      "effort": "L",
      "difficulty": "hard",
      "requirements": ["R2", "R3", "R4"],
      "depends_on": [1],
      "commit": "test(pg): exact-value and no-stall controls for live checkpointing"
    },
    {
      "phase": 3,
      "focus": "epoch_pg/src/event_bus: the fix. Add SubscriberState.contiguous_event_id (plain Uuid) and SubscriberState::new_with_event_id, keeping the existing new() for its 21 call sites; add last_event_id to the start_listener seeding SELECT as (i64, Option<Uuid>) mapping None to nil. In process_subscriber_for_batch: both per-event sites call record_processed instead of update, seeding the PendingCheckpoint eagerly from state.contiguous_checkpoint and its paired id; the contiguous branch switches from direct field assignment to update(); KEEP the None arm, which is the publishable advance constructor; move the try_flush_pending_checkpoint call OUT of the 'new_contiguous > contiguous_before' branch so it runs unconditionally under a !replay_always guard; set cached_checkpoint ONLY from local_cache (delete the unconditional cached_checkpoint = Some(new_contiguous) on advance); add debug_assert!(pending <= state.contiguous_checkpoint) immediately before the flush. Apply the publishable predicate INSIDE the take_if closure in try_flush_pending_checkpoint and inside the selection/filter in flush_expired_checkpoints and flush_all_pending_checkpoints -- applying it after the take_if or after the map remove silently DESTROYS the pending and loses first_event_time. Do not merge the two advancers; do not touch advance_catchup_prefix or the public update_checkpoint.",
      "effort": "M",
      "difficulty": "hard",
      "requirements": ["R1", "R3", "R4", "R5", "R6", "R7"],
      "depends_on": [1],
      "commit": "fix(pg): publish only contiguous checkpoints from the live listener path"
    },
    {
      "phase": 4,
      "focus": "epoch_pg/tests: regression tests that must FAIL with phase 3 reverted. test_live_shutdown_does_not_publish_above_held_hole (PRIMARY; default Synchronous, commented as such; also asserts the persisted last_event_id equals the below-hole event id, the only DB-level R5 check). test_live_batched_flush_does_not_publish_above_held_hole (Batched batch_size 1000 / max_delay_ms 300; ONE unavoidable fixed sleep >= 2x the hard-coded 1s flush_interval, taken AFTER delivery is verified, with a comment forbidding conversion to a poll loop). test_live_deser_skip_above_hole_does_not_publish_above_hole (raw inserts with garbage data; the delivery poll is impossible here since the deser branch continues, so captured_logs_contain_since on the WARN is its SUBSTITUTE signal; must state its CheckpointMode and flush trigger explicitly). All on the isolated table, gap_timeout 30s, exact-equality against a committed below-hole sequence, observe-then-release-then-assert ordering. Plus a ReplayAlways guard. Verify each fails with phase 3 stashed.",
      "effort": "M",
      "difficulty": "hard",
      "requirements": ["R1", "R5", "R6"],
      "depends_on": [2, 3],
      "commit": "test(pg): pin the live path against publishing above a held hole"
    },
    {
      "phase": 5,
      "focus": "Docs and hygiene. CHANGELOG ### Fixed entry SCOPED per spec section 6: silent loss closed on shutdown/reconnect in all modes and on the Batched timer tick, with the gap_timeout-bounded readiness delay attributed ONLY to Batched -- do NOT claim a steady-state change on the default Synchronous config, which already held at a hole. Rustdoc on record_processed, both PendingCheckpoint constructors, the publishable predicate, the new SubscriberState field and new_with_event_id. Extend flush_checkpoint's hazard rustdoc to cite spec 0027 R1. Mark the spec Implemented with an Implementation Summary in spec 0026's style.",
      "effort": "S",
      "difficulty": "standard",
      "requirements": ["R7"],
      "depends_on": [4],
      "commit": "docs(pg): record the live-path contiguous checkpoint fix"
    }
  ]
}
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
- Suppressing a flush whose position equals the last *cached* persisted value (§4 Q2). Note the narrower redundant-write case is **no longer** out of scope: §4 Q1's seed-based predicate eliminates it as a side effect.
- Making `flush_interval` configurable (§7.2; public-behaviour change).
- CLOUD-227 (`ReplayAlways` HWM loss window), CLOUD-231 (duplicate-subscriber first-wins in the priority-group dedup via `seen_sids`).
- Testing the reconnect flush trigger (§7.4).

## 11. Residual uncertainty

Stated as uncertainty rather than papered over:

- **Lost-outcome direction.** Task inputs are built by draining `subscriber_states.remove(sid)` and `pending_checkpoints.remove(sid)` per subscriber, deduplicated through `seen_sids` (~1558). Because the pending value is *removed* from the map and handed to the task, an outcome that never returns loses the in-memory pending checkpoint. INFERRED to fail **safe** (a lost pending means a checkpoint is not advanced, rather than advanced too far), and the code reads that way, but this was not proven by test. If a reviewer wants it proven, it is a separate exercise.
- **`inline_dispatch_integration_tests.rs` / `transaction_integration_tests.rs`** are INFERRED unaffected (§7.5), not verified.
- **The fence-cleared control's flakiness rate under real CI load is unknown**, which is exactly why §7.3 now ships it `#[ignore]`d rather than gambling on it. The residual risk is the opposite of the original one: an ignored test decays silently, so if the fence route ever regresses, `test_rolled_back_gap_fence_clears_without_record` is the only thing standing between that regression and a green suite.
- **P1's unused `pub(crate)` items.** `record_processed` and the predicate land in P1 with no callers. Believed safe under `-D warnings` because they are exercised by `#[cfg(test)]` unit tests in the same crate, which suppresses `dead_code`. Stated as belief, not verified by compiling, since this spec was written without running `cargo`. If P1's clippy gate fails for this reason, fold P1 into P3 rather than adding an `#[allow]`.
