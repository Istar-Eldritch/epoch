# Spec 0026: contiguous catch-up checkpoint

**Issue:** CLOUD-226 · **Status:** Draft · **Crates:** `epoch_pg`
**Scope:** `fix(pg)` · no migration, no schema change, no public API change.
**Found by:** round-1 concurrency review of the spec 0024 work.
**Supersedes:** spec 0024 OQ-4, which named this hazard and deferred it.
**Related:** CLOUD-227 (`ReplayAlways` HWM residual, out of scope here).

> Anchors in this document are **symbol names, not line numbers**. An earlier draft
> cited line numbers and every one of them had drifted by 40-135 lines within days.

---

## 1. Problem

The catch-up path persists a checkpoint it has not earned, so a committed event can be
delivered to nobody while readiness reports success.

`global_sequence` is assigned by a non-transactional `nextval()` (spec 0019), so a page of
committed rows can legitimately contain a hole that fills in later: sequence 4 claimed by a
still-open transaction, sequence 5 already committed and visible.

`catch_up_from_checkpoint` walks pages of `global_sequence > checkpoint` and, for every row it
*sees*, advances a linear `PendingCheckpoint` via `record_catchup_progress`, then flushes the
maximum. Given `1,2,3,5` it persists `5`. The listener seeds `SubscriberState::new(5)` and every
later batch queries `global_sequence > min_checkpoint`. Event 4 commits moments later and is
never read by anyone: no WARN, no `epoch_event_bus_gap_timeouts` row, no DLQ entry.
`wait_until_caught_up` returns `Ok(true)`, so a readiness gate certifies a read model that is
silently missing an event.

**There are two such writers, not one.** `subscribe()` calls `catch_up_from_checkpoint` and then
runs a *second*, separately coded buffer-drain pass that also calls `record_catchup_progress`
per row and flushes a linear maximum. `flush_checkpoint` is a blind
`DO UPDATE SET last_global_sequence = EXCLUDED.last_global_sequence` and is **not monotonic**,
so the drain overwrites a conservative checkpoint inside the same `subscribe()` call. Fixing
only `catch_up_from_checkpoint` leaves the bug fully reachable on the `subscribe()` path.

**What changed, and why this is now worth fixing.** The hazard predates this work inside
`subscribe()`, where it fires once per subscriber. The R2 pass added in spec 0024 runs the same
unfenced catch-up over *every registered subscriber on every* `start_listener()`. The blast
radius moved from once-per-subscription to once-per-boot, and a boot under write load, a rolling
restart, is exactly when in-flight transactions are most likely.

## 2. Goals / Non-Goals

**Goals.** Neither catch-up writer may persist a checkpoint above a hole. Termination must be
preserved when a hole never fills during the pass.

**Non-Goals.** No change to the live path's gap machinery. No change to public API, error types
or schema. Not fixing the `ReplayAlways` HWM (CLOUD-227), head-of-line blocking (CLOUD-228),
`shutdown()` bounding (CLOUD-229) or the trigger leak (CLOUD-230).

## 3. Rejected design: reusing the live path's fence

The obvious approach, and the one an earlier draft of this spec specified, is to give catch-up a
`SubscriberState` and call `advance_contiguous_checkpoint` per page, reusing the snapshot fence
and the `gap_timeout` backstop. **It is unsound, and worse than the bug.**

`advance_contiguous_checkpoint` decides gap-ness from the visible-sequence set it is handed. Its
unstated precondition is that the set is *complete* from `contiguous_checkpoint` upward. The
live path satisfies this by construction: it re-queries from `min_checkpoint`, i.e. from *below*
the gap, on every batch, so the gap region is re-observed until it fills.

Catch-up cannot satisfy it. Its pagination cursor must advance past the hole (see §4.1), so
later pages query `global_sequence > cursor` and **structurally cannot contain the gap sequence
even after it commits**. The resolver then sees a permanent hole, and once the holding writer
commits and `xmin` passes the captured `fence_xmax`, it reports `SkipReason::FenceCleared`,
which the system defines as a proven-lossless rollback and logs at `debug!` with no record and
no callback. A *committed* event is skipped, permanently, more quietly than today.

The fence proves the writer **finished**, not that it **aborted**. That inference is only valid
against a current view of the gap region, which catch-up does not have.

Two further findings pointed the same way:

- The `gap_timeout` backstop cannot fire inside a bounded pass at all. A gap is only *recorded*
  on first observation; fence and backstop verdicts require a *later* call. A pass that ends at
  head never makes one, at any `gap_timeout`.
- Threading `SubscriberState` through catch-up would put one `HashSet<u64>` entry per event
  above the hole, undrained until the checkpoint advances. On a cold read model that is a
  backlog-sized allocation on the boot path. `SubscriberState`'s own doc bounds
  `processed_ahead` by concurrent uncommitted transactions, which would no longer be true.

## 4. Design: a contiguous-prefix counter

Catch-up does not need to resolve gaps. It needs to stop lying about what it processed. The live
loop is entered immediately afterwards and already owns gap resolution, with persistent state
and a working fence.

### 4.1 Separate the pagination cursor from the persisted checkpoint

- **Pagination cursor** keeps advancing by *max sequence seen in the page*. If it advanced only
  contiguously, a hole that outlives the pass would make catch-up re-read the same page forever.
- **Persisted checkpoint** becomes the highest contiguous prefix.

### 4.2 The prefix counter

Rows arrive in ascending `global_sequence` order, so the prefix is a running counter, not a set:

- Seed `contiguous` from the checkpoint the pass started at.
- Per processed row, if `seq == contiguous + 1`, set `contiguous = seq` and remember that row's
  `event_id`. Otherwise stop advancing `contiguous` for the remainder of the pass.
- Keep paginating to head regardless, processing rows as today.
- Flush once at the end of the pass, from `contiguous` and its remembered `event_id`.

O(1) memory. No `SubscriberState`, no `TxidSnapshot`, no fence, no backstop, no shared gap
side-effect handler, and no per-page `seq -> event_id` map.

A hole that fills mid-pass is simply not detected. That is deliberate: the checkpoint stays
below it, and the live loop re-reads from there and delivers it. Detecting it would buy only the
suppression of a re-delivery the system already permits (§4.4).

### 4.3 The `subscribe()` buffer drain gets the same treatment

The drain continues from where catch-up stopped and must not undo it. `catch_up_from_checkpoint`
returns the **pagination cursor**, so the drain's `> current_sequence` lower bound stays correct
and it does not reprocess the whole backlog; it must additionally receive the **contiguous**
value, continue the same counter over the drained range, and flush only that. Because
`flush_checkpoint` is not monotonic, "flush only the contiguous value" is a requirement, not an
optimisation.

### 4.4 At-least-once above a hole is the existing contract

The listener re-seeds from the *persisted* checkpoint, so events processed above a gap are
re-delivered after any restart. Re-delivery above a hole is therefore pre-existing accepted
behaviour, not something this spec introduces.

### 4.5 `ReplayAlways` is unchanged, and remains wrong

`record_catchup_progress` early-returns to the in-memory HWM and never writes the checkpoints
table, so the prefix counter does not apply. Note the HWM is **not** a "replays from zero every
boot" story, as an earlier draft claimed: `catch_up_from_checkpoint` resumes a `ReplayAlways`
subscriber from its *surviving* HWM, a linear maximum, so such a subscriber above a hole misses
the event on an in-process listener restart and reports ready at the higher position. Left as is
and tracked in CLOUD-227, because making the HWM contiguous changes readiness timing for every
`ReplayAlways` subscriber.

## 5. Requirements

- **R1.** `catch_up_from_checkpoint` MUST NOT persist a checkpoint above a hole.
- **R2.** The `subscribe()` buffer drain MUST NOT flush a checkpoint above the contiguous value
  carried over from catch-up.
- **R3.** Both MUST terminate when a hole is held open for the entire pass.
- **R4.** A checkpoint's `last_event_id` MUST correspond to its `last_global_sequence`.
- **R5.** Readiness MUST NOT report caught-up while a subscriber's checkpoint is legitimately
  held below a hole. This is an observable behaviour change: a gate that previously returned
  `true` over a lost event now blocks until the hole resolves.
- **R6.** `ReplayAlways` behaviour unchanged.
- **R7.** No public API, error-type or schema change.

## 6. Test Plan

Two facts shape this plan.

**Absolute sequence numbers are unusable.** `epoch_events` and its sequence are shared by five
parallel test binaries, and `RESTART IDENTITY` truncation is issued from only one of them.
`#[serial]` serialises within a binary, not across processes. Every assertion must be relative
to sequences captured via `INSERT ... RETURNING global_sequence`, exactly as
`test_in_flight_transaction_gap_is_held` already does.

**The open-transaction pattern already exists.** That test opens `pool.begin()`, claims a
sequence without committing, commits a later one on another connection, asserts the checkpoint
does not pass the held sequence, then commits and asserts delivery. Reuse it and its
`start_fence_test_bus` / `insert_committed_event` helpers rather than reinventing them. It is
also a known load-sensitive test built on fixed multi-second sleeps, so its *shape* should not be
replicated five times.

Accordingly, prefer unit tests over integration tests where the unit suffices:

1. **Unit: prefix counter stops at the first hole.** `catch_up_from_checkpoint` is `pub(crate)`.
   Assert the persisted checkpoint sits below the held sequence, and that the returned
   pagination cursor is above it. Deterministic, no sleeps. (R1, R3)
2. **Unit: multi-page.** `catch_up_batch_size` of 2 with the hole on a non-final page, proving
   the counter survives page boundaries and does not resume advancing after the hole. (R1)
3. **Unit: positive control.** No hole: the checkpoint advances to head, with `last_event_id`
   matching. Without this, an implementation that never advances passes every other test. (R1, R4)
4. **Integration: end-to-end delivery.** Hold a transaction across a `start_listener()` catch-up,
   commit it, assert the subscriber receives the event and readiness then reports caught-up. Use
   a bounded `wait_until_caught_up`, not a fixed sleep. This must subscribe *before* starting the
   listener, or it exercises `subscribe()`'s catch-up rather than the R2 pass that motivated the
   fix. (R1, R5)
5. **Integration: `subscribe()` drain does not overwrite.** Subscribe with a hole held open and
   buffered events above it; assert the persisted checkpoint is still below the hole after
   `subscribe()` returns. This is the test that would have caught the second writer. (R2)
6. **Unit: `ReplayAlways` unchanged.** Regression guard only; it passes with and without the fix,
   so it does not count toward proving anything. (R6)

All parallel-safe, idempotent and independent per CLAUDE.md §8, with stable channel names so
they do not leak NOTIFY triggers (CLOUD-230).

## 7. Failure Modes

- A hole held for the entire pass leaves the checkpoint below it, so the live loop re-reads and
  re-delivers the above-hole events. Accepted: identical to the restart contract (§4.4).
- A long-running transaction pins the hole indefinitely, so the checkpoint stops advancing and
  readiness blocks. Correct, and newly visible: the live path's `gap_timeout` backstop still
  fires and records it, since the live path retains state across batches. Catch-up itself
  contributes no gap-timeout rows, by design (§3).
- A cold read model with a hole near the start re-delivers a large backlog on the next boot.
  Bounded by the hole's lifetime, which the live path's backstop bounds in turn.
- `ReplayAlways` retains its own loss window (§4.5, CLOUD-227).

## 8. Files Changed

- `epoch_pg/src/event_bus/mod.rs` — `catch_up_from_checkpoint` (prefix counter, return the
  cursor, flush contiguous), its `record_catchup_progress` usage, and the `subscribe()`
  buffer-drain pass.
- `epoch_pg/tests/pgeventbus_integration_tests.rs` — tests 4 and 5; unit tests 1-3 and 6 live
  beside the code in `mod.rs`.
- `CHANGELOG.md` — one `### Fixed` entry, noting the R5 readiness behaviour change.
- `specs/0024-cloud221-subscriber-readiness.md` — mark OQ-4 resolved here.

## 9. Acceptance Criteria

1. All new tests pass; existing gap-fence and gap-timeout tests pass **unchanged**, proving the
   live path was not touched.
2. Reverting the fix makes tests 1, 2 and 5 fail. Verified mechanically by stashing the source
   change and re-running, not asserted by inspection.
3. `cargo clippy --all-targets -p epoch_pg -p epoch_core -- -D warnings` clean;
   `cargo fmt --check` clean.
4. The event-bus binary passes in isolation and under a concurrent writer on `epoch_events`, on
   consecutive runs. Note `cargo test --workspace` green twice is a weak gate on its own: the
   suite shares one database across five binaries and contains at least one known load-sensitive
   test, so a green pair proves less than it appears to.

## 10. Open Questions

- **OQ-1.** Should catch-up report that it stopped below a hole, so a readiness gate can
  distinguish "caught up" from "caught up to a hole"? Deferred: additive, and R5 already makes
  the gate honest by blocking rather than lying.
- **OQ-2.** Does any consumer depend on the old linear-max checkpoint? In-repo, the only
  consumer of `catch_up_from_checkpoint`'s return value is `subscribe()`, which §4.3 handles
  explicitly. Out-of-repo consumers cannot depend on it: the function is `pub(crate)`.
