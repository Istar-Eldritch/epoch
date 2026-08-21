# Brief: CLOUD-232, live listener path persists a checkpoint above an open hole

Input for the spec-writer. This is a technical brief, not a spec: it states the problem, the
verified mechanism, the design that discovery converged on, and the open questions the spec must
settle. Everything marked VERIFIED was read in source on `main` at `ec378d3`; everything marked
INFERRED still needs proving.

## 1. Problem

`epoch_events.global_sequence` is assigned from a **non-transactional** `nextval()`. A reader can
therefore see sequences 5 and 7 while 6 is still claimed by an open, uncommitted transaction.
That missing sequence is a **hole**.

If a subscriber persists a checkpoint above a hole, then when the holding transaction commits, its
event sits below the checkpoint and is delivered to **nobody**: no warning, no DLQ row, no
gap-timeout record, and readiness still reports success. Silent, unrecoverable loss of a committed
event.

CLOUD-226 (spec `0026-cloud226-contiguous-catchup-checkpoint.md`, merged) fixed this for the
one-shot catch-up pass and for the buffer drain inside `subscribe()`: both now persist only the
highest **contiguous prefix**, while the pagination cursor still advances by max-seen so the pass
terminates.

CLOUD-232 is that **the same failure mode survives on the live listener path**, which CLOUD-226
deliberately scoped out. The guarantee is therefore not end-to-end: a bus can catch up
conservatively and then have the bad value written underneath it moments later.

## 2. Verified mechanism

All in `epoch_pg/src/event_bus/`.

1. In `process_subscriber_for_batch` (`mod.rs`), `pending_checkpoint` is updated with **every
   processed event's own sequence**, unconditionally, on both the deserialization-skip path and
   the success path, via `PendingCheckpoint::update`. That value is max-seen, not contiguous.
2. It is only corrected to the contiguous value inside the branch guarded by
   `if new_contiguous > contiguous_before`. That branch uses **direct field assignment**
   (`p.global_sequence = new_contiguous; p.event_id = checkpoint_event_id;`), not `update()`.
3. So when a batch contains a hole and the prefix does **not** advance, the branch never runs and
   `pending_checkpoint` survives as `Some(max_seen)`, above the hole.
4. `pending_checkpoint` is returned in `SubscriberBatchOutcome` and stored in the listener's
   `pending_checkpoints` map. Critically it is **carried across batches**: the listener does
   `pending_checkpoints.remove(sid)` and feeds it back in as a parameter, so a stale max-seen value
   persists in the map until something flushes it.
5. Two sites then write it, both calling `flush_checkpoint` with **no contiguity check**:
   - `flush_all_pending_checkpoints` (`checkpoint.rs`): no mode check either. Runs on **reconnect
     and shutdown, in every checkpoint mode**.
   - `flush_expired_checkpoints` (`checkpoint.rs`): early-returns unless `Batched`, then flushes
     any entry whose `first_event_time.elapsed() >= max_delay`.

`flush_checkpoint` is a blind, non-monotonic upsert, and its own rustdoc already states the
contract these sites violate: "Every caller MUST pass only a value it is willing to publish as the
subscriber's current position."

### Severity, refined

- Under `Synchronous` (**the default**), `flush_expired_checkpoints` early-returns, so the leak
  fires only on **reconnect or shutdown**.
- Under `Batched`, it fires **routinely**, every `max_delay_ms`.

The exposure window is not "a batch that races a hole" but "any batch that ever processed above a
held hole, until the next flush of any kind".

## 3. What is NOT wrong, and must not be broken

`advance_contiguous_checkpoint` (`subscriber_state.rs`) is the authoritative advancer of
`state.contiguous_checkpoint`, and it is correct. It already folds in the **deliberate skip** of an
abandoned hole: it walks past a gap when the fence is provably cleared
(`snapshot.xmin >= fence_xmax`, proving the writer aborted) or when the timeout backstop fires
(`gap_duration > gap_timeout`), and otherwise holds.

Consequence, and this narrows the ticket as filed: **publishing only `contiguous_checkpoint` cannot
permanently stall a subscriber.** The skip logic lives inside the counter, not beside it. The
ticket's warning about stalling on rolled-back transactions was my error and the spec should not
repeat it.

`try_flush_pending_checkpoint` (`mod.rs`) is already correctly shared by the catch-up and live
paths and needs no change. The defect is entirely in what the live path feeds it.

The two paths' advancers **cannot** be unified: `advance_catchup_prefix` is a single-event exact-match
(`seq == contiguous + 1`) advance with no fence, no gap timeout and no processed-ahead set, by
design (spec 0026 §3), whereas `advance_contiguous_checkpoint` batches, fences via `TxidSnapshot`
and times out stuck gaps. Do not propose merging them.

## 4. Root defect

`PendingCheckpoint` has four fields serving **two different jobs**:

| Field | Job | Written by |
| --- | --- | --- |
| `global_sequence`, `event_id` | the position to publish | `new()`, `update()`, direct assignment |
| `events_since_checkpoint` | `Batched` `batch_size` threshold | `update()` only |
| `first_event_time` | `Batched` `max_delay_ms` threshold | `new()` only, never refreshed |

`update()` **conflates** them: it moves the published position *and* bumps the counter. The per-event
calls exist for the accounting; moving the position is collateral damage.

So the fix is to split "record that an event was processed" from "record what may be published". It
is not to change the counter's cadence logic.

### Why the naive deletion is wrong

Simply deleting the two per-event `update()` calls leaves **nothing** incrementing
`events_since_checkpoint`, because the contiguous branch uses direct field assignment. The counter
would freeze at 1 and the `batch_size` trigger would go permanently dead, silently reducing
`Batched` to a `max_delay_ms`-only cadence. Not data loss, but it defeats the feature.

## 5. Design that discovery converged on

Publish the **contiguous** position; count **every** processed event.

1. `checkpoint.rs`: add a method that bumps `events_since_checkpoint` **without** touching the
   position. Ahead-of-gap events must still count toward the `Batched` threshold, since they will
   be redelivered from whatever is persisted, but must never *become* the published value.
2. Both per-event sites in `process_subscriber_for_batch` call that method instead of `update()`,
   seeding the `PendingCheckpoint` on first use from `state.contiguous_checkpoint` (see §6 Q1 for
   the paired `event_id`). `state.contiguous_checkpoint` is stable for the whole loop, since it is
   only mutated later by `advance_contiguous_checkpoint`, so it is a safe seed at any point.
3. The contiguous branch switches from direct assignment to `update()`, so an advance bumps the
   counter too, and records the paired `event_id` for the next seed.
4. **Move the `try_flush_pending_checkpoint` call out of the `if new_contiguous > contiguous_before`
   branch** so it runs unconditionally (still `!replay_always`-gated). Today the only call site is
   inside the branch a held hole prevents from running, so a `Batched` threshold crossing cannot
   flush while a hole is open. Without this move the rest of the fix is cosmetically correct but the
   cadence regression of §4 persists.

**Why eager seeding rather than making the position optional.** There is always a legitimately
publishable contiguous value, namely the current one, usually equal to what is already persisted, so
re-publishing it is idempotent. That keeps the field non-optional, needs no `None`-skipping in either
flush site, and preserves `max_delay_ms` measuring from the first unflushed event rather than
restarting at the first advance. An earlier `Option<u64>` proposal was dropped for this reason.

**Counter self-bounds.** With eager seeding, a threshold trip while a hole is held flushes the seed
(a harmless idempotent write of the already-persisted value) and resets the counter. The cost is
periodic redundant writes for as long as a hole is held, not unbounded growth. See §6 Q2.

**No public API or schema change.** `PendingCheckpoint` and `SubscriberState` are `pub(crate)`;
`CheckpointMode`, `should_flush_checkpoint` and the checkpoints table are untouched.

## 6. Open questions the spec must settle

**Q1. Where does the seed's paired `event_id` come from?** R4 in spec 0026 requires
`last_event_id` to match `last_global_sequence`. `state.contiguous_checkpoint` currently has no
paired id: `SubscriberState` stores only the sequence, and the query that seeds it
(`mod.rs`, subscriber-state initialization) selects **only** `last_global_sequence`. Discovery
proposes adding a `contiguous_event_id` field plus `last_event_id` to that `SELECT`. The column is
nullable `UUID` (`m003_create_event_bus_infrastructure.rs`), so this needs no migration. The spec
must decide whether a brand-new subscriber seeding at `(0, nil)` may ever be flushed, since that
would create a checkpoint row at sequence 0 where none existed. It should be harmless (a 0 row and
a missing row both mean "replay from the start"), but it must be stated, not assumed.

**Q2. Suppress redundant idempotent writes?** A cheap option is for
`try_flush_pending_checkpoint` to skip when the pending position equals the cached persisted value,
using the existing `checkpoint_cache`. Decide whether this is in scope or a separate optimisation.

**Q3. Guard, or trust the producers?** Once the producer side is fixed, every value that can occupy
`pending_checkpoints` is provably contiguous, so a runtime guard would validate something that
structurally cannot happen, which CLAUDE.md discourages. Discovery suggests a `debug_assert!`
inside the live path rather than a runtime `WHERE` clause. Note this is **not** the previously
rejected proposal: a monotonic `WHERE` guard on `flush_checkpoint` itself was rejected because it
would break the legitimate deliberate-rewind feature via the public `update_checkpoint`, and that
path never populates `pending_checkpoints`. The spec should decide explicitly and record the
distinction.

**Q4. ReplayAlways.** `process_subscriber_for_batch` already nulls both `pending_checkpoint` and
`cached_checkpoint` for a `ReplayAlways` subscriber so the listener's periodic and shutdown flushes
cannot defeat replay-from-zero. Confirm the new unconditional flush call site preserves that, and
state it as a requirement.

## 7. Requirements the spec should number

One consequence to state as a requirement in its own right: because per-subscriber state is seeded
from the **persisted** checkpoint when the listener wakes, a poisoned checkpoint becomes the next
boot's seed. The loss is permanent, not transient. That is what makes this P1 rather than a
missed-delivery annoyance.

- The live path must never persist a checkpoint above a sequence it has not proven contiguous,
  including via `flush_expired_checkpoints` and `flush_all_pending_checkpoints`.
- A hole abandoned by a rolled-back transaction must still be skipped, via both the fence-cleared
  and timeout-backstop routes, so no subscriber stalls permanently.
- `Batched`'s `batch_size` trigger must remain functional, including while a hole is held.
- `max_delay_ms` must continue to measure from the first unflushed event.
- `last_event_id` must always match the published `last_global_sequence` (spec 0026 R4).
- `ReplayAlways` subscribers must continue to write no checkpoint from this path.
- No public API change, no schema change, no migration.

## 8. Test plan

### 8.1 Two decisions that shape everything else

**Use a private events table per test.** `ReliableDeliveryConfig::events_table` is public and
`setup_trigger` already supports a custom table. The unit-test helper `cu_isolated_events_table`
(`mod.rs`, `#[cfg(test)]`) does `CREATE TABLE ... (LIKE epoch_events INCLUDING ALL)` with its **own
sequence**; the integration binary needs its own copy, since that one is not reachable from
`tests/`. Note `LIKE` does **not** copy triggers, so `setup_trigger()` must run before
`start_listener()`, with a `Uuid`-unique channel name.

This buys three things: the sequence is no longer shared, so **exact-equality** assertions on the
persisted checkpoint become possible; `bus_name` is `config.events_table`, so checkpoint and
gap-timeout rows are per-test; and no `truncate_epoch_tables` call is needed, so these tests
contribute nothing to the `ACCESS EXCLUSIVE` contention that caused the five-hour wedge.

**Assert exact equality, not `< seq_hole`.** The existing near-miss uses
`checkpoint.is_none() || matches!(checkpoint, Some(s) if s < seq_n)`, which **passes when nothing
was ever written**, including when a sibling binary's `TRUNCATE` wipes the row mid-test. Committing
an event *below* the hole first gives a known-good value to assert equality against, which
simultaneously proves the leak is closed, proves the write path is alive, and proves the fix did not
regress the checkpoint backwards. Make this a review gate for the spec.

### 8.2 Priority: the default mode makes shutdown the production case

`ReliableDeliveryConfig::default()` sets `checkpoint_mode: Synchronous`, and
`flush_expired_checkpoints` early-returns for `Synchronous`. So on default configuration the leak is
reachable **only via shutdown or reconnect**, and the `Batched` timer-tick path is opt-in. The
shutdown test is therefore the primary one, mapping onto the real incident shape (a rolling restart
under write load). My earlier ordering had this backwards.

### 8.3 Regression tests

All share: isolated table, `gap_timeout: 30s` so the backstop cannot fire inside the window, a
committed event *below* the hole, `claim_hole_uncommitted` to open the hole, two committed events
above it, and a **bounded poll** proving the above-hole events were delivered (which is what proves
`pending_checkpoint` was populated with max-seen). Ordering discipline, inherited from CLOUD-226 and
mandatory: capture the observation, then roll back or commit the held transaction and shut down,
**then** assert. An `assert!` that unwinds with the transaction open is exactly the shape that
produced the wedge.

1. `test_live_shutdown_does_not_publish_above_held_hole` — **primary**. Default `Synchronous`,
   stated explicitly in a comment since the point is that only the shutdown flush can leak.
   `shutdown()` with the transaction still open, then read. Asserts the checkpoint equals
   `seq_below`. Pre-fix reads `Some(seq_above2)`.
2. `test_live_batched_flush_does_not_publish_above_held_hole` — targets `flush_expired_checkpoints`
   with `Batched { batch_size: 1000, max_delay_ms: 300 }` (batch_size high so only the delay can
   fire).
3. `test_live_deser_skip_above_hole_does_not_publish_above_hole` — the deserialization-skip branch
   is the **second** unconditional write site and no existing test covers it above a hole. Write the
   above-hole events as raw inserts with garbage `data` so deserialization fails, and assert via
   `captured_logs_contain_since` that the WARN actually fired, otherwise the test is vacuous.

**One fixed sleep is unavoidable in test 2** and the spec must say so, or a reviewer will "clean it
up" into a poll loop that cannot fail: "checkpoint has not advanced" is both the expected
pre-condition and what a stalled listener looks like, so there is no positive edge to poll for.
Mitigate by verifying delivery *before* the sleep starts, so the sleep only has to cover the flush
tick, and size it at >= 2x `flush_interval`. Note `flush_interval` is a hard-coded
`Duration::from_secs(1)` in `start_listener`, so every timer-tick test is floored at ~1s; making it
configurable is a public-behaviour change and out of scope.

### 8.4 Positive controls: the fix must not stall a subscriber

- `test_backstop_hole_still_advances_checkpoint` — **mandatory**, and the one to trust. With
  `snapshot_fencing: false` and `gap_timeout: 500ms` it depends only on elapsed wall-clock, so it
  has zero load sensitivity. Asserts the checkpoint reaches `>= seq_above` (`>=`, since after a
  backstop skip the prefix legitimately runs to head) and that a gap-timeout row exists.
- `test_fence_cleared_hole_still_advances_checkpoint` — same shape with `snapshot_fencing: true`,
  `gap_timeout: 30s` so a pass cannot be the backstop in disguise, asserting **no** gap-timeout row.
  **Load-sensitive and an isolated table does not help**, because clearing needs
  `snap.xmin >= fence_xmax` and `xmin` is instance-wide: any transaction open anywhere on the server
  pins it. Sibling tests in this very suite hold transactions for 4s+. Also note `batch_snapshot` is
  only queried once a gap is already active, so `fence_xmax` is `None` on first observation and
  backfilled on the next batch, giving a two-tick (~2s) floor. Discovery recommends it **fail**
  rather than report inconclusive, on the grounds that an inconclusive positive control cannot tell
  a stalling fix from a working one. Pair it with the backstop control so one hard signal always
  remains.
- Harden `test_batched_checkpoint_flushes_at_batch_size` and `test_batched_checkpoint_flushes_at_max_delay`
  from `assert!(checkpoint.is_some())` to exact-value assertions on an isolated table. This is the
  control that catches the §4 counter regression, and it converts two currently decorative tests
  into real ones.
- Unit tests in `checkpoint.rs` if the fix threads a contiguity bound into the flush helpers: a
  pending above the bound is skipped, one at or below is written unchanged, the counter survives.
  These need no database and will still mean something in five years.

### 8.5 Reconnect is knowingly untested

The reconnect flush is reached only from `WakeReason::Notification(Err(e))`, which needs the
listener's `PgListener` connection killed externally via `pg_terminate_backend`. That is racy to
target and a cross-connection kill on a shared instance, and it is redundant: it is the **same
function with the same missing guard** as the shutdown path, so test 1 covers the defect and only
the trigger differs. The spec should record this as a deliberate gap rather than leave it unremarked.

### 8.6 Existing tests

**No existing test encodes the bug as expected**, verified across `pgeventbus_integration_tests.rs`
and `saga_adapter_integration_tests.rs`. The four `assert!(checkpoint.is_some())` tests are
insensitive in both directions; the two lower-bound assertions
(`test_in_flight_transaction_gap_is_held` post-commit, `test_rolled_back_gap_fence_clears_without_record`)
are cases where the prefix legitimately reaches the asserted value, and the second doubles as an
existing positive control. Not audited: `inline_dispatch_integration_tests.rs` and
`transaction_integration_tests.rs`, INFERRED unaffected because the inline-dispatch path does not go
through `process_subscriber_for_batch`.

Known load-sensitive, both worth flagging to whoever reviews the fix:
`test_in_flight_transaction_gap_is_held` (a bare 4s sleep that can pass **vacuously** under load,
plus the suite's longest transaction hold and so its largest contribution to `xmin` pinning) and
`test_rolled_back_gap_fence_clears_without_record` (its 6s sleep must cover two ticks *plus*
instance-wide `xmin` advancing; a failure there after this change is more likely a concurrent binary
than the fix).

Test pools set `lock_timeout = '30s'` and `idle_in_transaction_session_timeout = '60s'`, so any
test holding a hole open must stay well inside 60s.

## 9. Blast radius

Enumerated by hand (the discovery worker assigned this timed out at 30 minutes with no usable
output; this was VERIFIED directly instead).

### Every `PendingCheckpoint` producer outside tests

| Site | Value written | Correct? |
| --- | --- | --- |
| `checkpoint.rs` `PendingCheckpoint::new` | caller's choice, counter starts at 1 | primitive |
| `checkpoint.rs` `PendingCheckpoint::update` | caller's choice, counter +1 | primitive, but conflates the two jobs (§4) |
| `mod.rs:283-284` `process_subscriber_for_batch`, deserialization-skip path | **max-seen** | **NO, this is the bug** |
| `mod.rs:321-322` `process_subscriber_for_batch`, success path | **max-seen** | **NO, this is the bug** |
| `mod.rs:491-496` `process_subscriber_for_batch`, contiguous branch | contiguous | value correct, but direct assignment so it never counts (§4) |
| `mod.rs:2791-2792` `advance_catchup_prefix` | contiguous | yes, this is the CLOUD-226 fix |

So the live path has exactly **two** offending sites, and the catch-up path is already correct.
The fix does not need to touch `advance_catchup_prefix`.

### Every writer to `epoch_event_bus_checkpoints`

1. `checkpoint.rs` `flush_checkpoint`, the blind non-monotonic upsert. Sole low-level writer,
   reached from three callers:
   - `try_flush_pending_checkpoint` (mode-gated via `should_flush_checkpoint`),
   - `flush_expired_checkpoints` (`Batched` only, no contiguity check),
   - `flush_all_pending_checkpoints` (all modes, reconnect and shutdown, no contiguity check).
2. `mod.rs:1728` the **public** `update_checkpoint`. This is the deliberate-rewind feature and is
   why a blanket monotonic guard on `flush_checkpoint` was rejected previously. It never populates
   `pending_checkpoints`, so it is untouched by this fix.
3. `mod.rs:3783` a `#[cfg(test)]` helper. Not shipped.

The surface is therefore small and fully accounted for: fixing the two producers plus the flush
call-site placement (§5.4) closes every path that can publish above a hole.

### Priority-group batching

Task inputs are built by draining `subscriber_states.remove(sid)` and `pending_checkpoints.remove(sid)`
per subscriber, deduplicated through a `seen_sids` set so a subscriber id registered twice is
processed once (the known CLOUD-231 first-wins behaviour, out of scope here). Because the pending
value is removed from the map and handed to the task, an outcome that never comes back loses the
in-memory pending checkpoint. INFERRED: that fails in the **safe** direction, since a lost pending
means a checkpoint is not advanced rather than advanced too far. The spec should confirm rather
than assume this.

### Remaining disagreement between persisted and in-memory state

After the fix, the persisted checkpoint can still legitimately **lag** `contiguous_checkpoint`
(between flushes, and under `Batched` by design). It must never **lead** it. That asymmetry is the
invariant the spec should state and the tests should pin.

## 10. Behavioural risk the spec must own

CLOUD-226's R5 already changed readiness semantics for the catch-up pass: a gate that used to report
caught-up over a lost event now blocks. Clamping the live path extends that to **steady-state
operation**. A subscriber sitting behind an open transaction will report not-caught-up for as long as
that transaction lives, bounded by `gap_timeout` rather than by the flush interval.

That is the correct behaviour, since the alternative is the silent loss this ticket exists to fix,
but it is a user-visible change and not merely an internal one. Consequences:

- Any caller with a tight `wait_until_caught_up` bound may newly time out while a long-running
  writer holds a transaction open. Existing bounds in the suite look generous enough (15s), but this
  is the mechanism to watch when the fix lands.
- It warrants a CHANGELOG entry mirroring the R5 one, not just a bug-fix line.
- The spec should state the bound explicitly: readiness can be delayed by at most `gap_timeout`,
  after which the backstop skips the hole and the prefix advances.
