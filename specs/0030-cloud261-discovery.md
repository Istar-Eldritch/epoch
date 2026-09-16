# CLOUD-261 Discovery: sequence-burn resilience for ReplayAlways subscribers (gap tolerance or burnless allocation)

## Source

Linear ticket CLOUD-261 (state Triage, priority 2, parent CLOUD-259), pinned against
worktree rev `bed756b` (`cloud-261` branch). Full ticket text fetched via
`linear issue view CLOUD-261` and reproduced below, followed by empirical verification
performed 2026-09-15 by four prober agents (GLM-5.3-flash) whose reports are listed as
verified evidence. The orchestrator spot-checked the reports' code citations against
source at `bed756b`; every line anchor the ticket cites holds.

### Ticket text

> ## Problem
>
> `nextval()` sequence allocation is non-transactional: any rolled-back event-insert
> transaction permanently burns a sequence number. Under suite/production load, burns
> are endemic (CLOUD-259 observed 3 distinct burns in one acceptance session: seqs
> 5003, 5014, 6706). A burn below an in-flight bootstrap deterministically wedges the
> sole `ReplayAlways`+fail-closed subscriber:
>
> 1. The subscriber's live batch observes the gap; the gap fence stays unproven under
>    sustained load (fence requires `xmin >= fence_xmax`, `subscriber_state.rs:337-363`),
>    the backstop refuses after `gap_timeout` (default 5s, `config.rs:388`), and
>    `GapUnproven` fires — from the live batch path only (`mod.rs:544-556`).
> 2. The ReplayAlways HWM advances only across unbroken prefixes (CLOUD-227 guard,
>    `mod.rs:3410-3424`), so the position pins below the hole forever.
> 3. P4b self-heal explicitly excludes ReplayAlways wedges (`mod.rs:1884-1893`: "the
>    remedy is a fresh `subscribe()`"), so the subscriber is inert until something
>    re-subscribes it.
>
> catacloud currently carries an application-level workaround for this entire chain
> (CLOUD-259 Phase 3, commit `614c06ae`: generation-chain halt-resilience with
> bounded-backoff re-subscribes). It works and is review-verified, but it is ~530 lines
> of integration-layer machinery compensating for a sequence-allocation artifact.
>
> ## Proposed options (either, or both)
>
> **A. Gap-skip-after-backstop for ReplayAlways subscribers.** Per-subscriber gap policy
> (`Halt` default | `SkipAfterBackstop` opt-in for fold-style ReplayAlways projections):
> after the backstop refuses and the fence is still unproven, log WARN, advance the HWM
> past the hole, and track skipped seqs so a late-materializing row at a skipped seq is
> detected (it would appear below HWM) and triggers a rebuild. Safety analysis needed
> for the in-flight-writer case — that is the reason `Halt` is the default; fold
> projections whose state is a pure function of *present* rows are the natural opt-in
> class.
>
> **B. Burnless sequence allocation.** Replace/augment `nextval()` with batched HWM
> allocation from a counter row inside the insert transaction (allocate N seqs per txn,
> cache per writer). Rollback then burns at most a batch boundary, only on crash;
> steady-state has zero gaps. Cost: counter-row contention (amortized by batching;
> nextval is lock-free precisely to be fast).
>
> ## Relationship to CLOUD-259
>
> * Either option makes the CLOUD-259 Phase 3 generation-chain heal unnecessary for the
>   burn class (it would remain as defense for genuine holes).
> * If A ships: catacloud opts its policy-graph projection into `SkipAfterBackstop` and
>   retires the heal machinery (or keeps it as belt-and-braces).
> * If neither ships: the Phase 3 workaround stands; see also the sibling unsubscribe/P5
>   ticket — the heal's safety depends on current wedge-inertness invariants
>   (`mod.rs:1884-1893`), which P5 would change.

## Verified evidence (probe and paper-test reports)

All four reports live in this worktree; all were produced against `bed756b` with
verbatim captures (command output quoted, code quoted with `path:line`), and the
orchestrator independently re-verified a sample of the code citations.

* `docs/probes/cloud261-failure-chain-paper-20260915.md` — desk-check of the failure
  chain: burn source, GapUnproven construction sites, HWM pin guard, P4b exclusion,
  fence/backstop/halt mechanics, persistence and recovery analysis.
* `docs/probes/cloud261-design-space-paper-20260915.md` — design-space map: allocation
  DDL and call sites, observable contracts, option A hook points and state, option B
  mechanics, CLOUD-227/232 prior art, gap-timeout audit machinery.
* `docs/probes/cloud261-burn-wedge-probe-20260915.md` — empirical reproduction: burn,
  wedge, late materialization, fresh-subscribe and release_halt recovery, plus a
  default-fencing control experiment.
* `docs/probes/cloud261-alloc-cost-probe-20260915.md` — allocation semantics measured
  in SQL: burn per variant, contention microbenchmark, crash bounds, ordering caveat.

## Findings beyond the ticket (all verbatim-evidenced in the reports)

### The wedge is conditional (refines the ticket's premise)

With default `snapshot_fencing: true` and a plain `nextval` burn (no long-lived
transaction), there is **no wedge**: the fence proves the burn via `FenceCleared` and
the position crosses the hole in ~1.1 s with zero halts (burn-wedge probe, Bonus
section; matches existing suite test
`test_fail_closed_gap_refusal_and_fence_cleared_recovery`,
`epoch_pg/tests/pgeventbus_integration_tests.rs:8432-8580`).

The wedge reproduces exactly as the ticket describes when the fence is **unavailable or
unproven**: (a) `snapshot_fencing: false` — one deterministic burn wedges a live
ReplayAlways+FailClosed subscriber: `GapUnproven` fired once 5.05 s after the gap
became visible (gap_timeout = 5 s), HWM pinned at seq 1 while head advanced to 6, all
post-wedge events undelivered; or (b) fence `xmin` pinned below `fence_xmax` past
`gap_timeout` by long in-flight writers — the sustained-load case CLOUD-259 observed.
The pinned-xmin case is precisely the "in-flight-writer safety question" option A must
be analyzed against. A wedged ReplayAlways subscriber is fetched by nothing: excluded
from the shared batch (`wedged_now` filter, `mod.rs:2074-2076`), from P4b
(`mod.rs:1891`), so with it as the only subscriber `compute_shared_floor` returns
`None` and the shared fetch is skipped entirely (`mod.rs:1955-1964`).

### Nothing persists or recovers a ReplayAlways wedge

`GapUnproven` writes nothing: no halt flag on disk, no `epoch_event_bus_gap_timeouts`
row (that table is fail-open backstop-skip only, `mod.rs:644-660`), no DLQ row, no
checkpoint row; the ReplayAlways HWM is in-memory only, "Never persisted"
(`mod.rs:1173-1178`). `release_halt` (`mod.rs:2290`) returns `Ok(())`, writes a
checkpoint row, fires `HaltReason::Released` — and **does not resume** a ReplayAlways
subscriber: the row it writes is never read back (the ReplayAlways live path routes
advancement to the in-memory HWM and never re-seeds from the persisted row,
`mod.rs:709-714`, `742-745`), so its WARN "delivery resumes from 3"
(`mod.rs:2325-2331`) over-promises for this mode. A fresh `subscribe()` delivers events
above the hole but **re-wedges at it** and **double-delivers** the seqs adjacent above
the hole (delivery counts `(3,2)`, `(4,2)`; catch-up pass vs live-path dedup gap,
`processed_ahead` guards only the live path, `mod.rs:359`) — "fresh subscribe()" is a
workaround with known defects, not a recovery path. The double-delivery may be a
distinct correctness bug worth its own ticket (prober Notes; not chased per
discipline).

### Late materialization is undetected today

A row committed **at** a skipped seq after the wedge is never detected: present in the
table, never delivered, position stays pinned, no new halt. No query ever reads below a
subscriber's position (all five fetches are `WHERE global_sequence > $1`); the only
below-observation in the tree is the documented operator JOIN
(`epoch_event_bus_gap_timeouts ⋈ epoch_events ON e.global_sequence = g.skipped_sequence`,
`mod.rs:958-970`). The detection machinery option A needs partially exists: the table,
its unique key, a `skipped_sequence` index, and `resolve_gap_timeout` are all in place;
today the table only records fail-open backstop skips, never fence-cleared or
refused/wedged paths.

### Option A hook points (from the design-space paper test)

* No per-subscriber gap policy exists today; `ReliableDeliveryConfig`
  (`epoch_pg/src/event_bus/config.rs:307/388`) carries only `gap_timeout` plus
  callbacks (`on_dlq_insertion`, `on_gap_timeout`, `on_halt`).
* The exact SkipAfterBackstop decision point is the `FailureMode::FailClosed` arm of
  the pure resolver `advance_contiguous_checkpoint`
  (`epoch_pg/src/event_bus/subscriber_state.rs:376-386`), whose single production
  caller is the `HaltReason::GapUnproven` firing site (`mod.rs:548-556`; the only such
  site in the crate, `mod.rs:553`).
* The reusable audit/metrics machinery: `GapTimeoutInfo` +
  `GapTimeoutCallback::on_gap_timeout` (`config.rs:163-241`) and the
  `epoch_event_bus_gap_timeouts` DDL (m009).
* `subscribe()` (`epoch_core/src/event_store.rs:247-252`) takes no start position — a
  rebuild is always a fresh subscribe from zero.

### Option B semantics and costs (from the alloc-cost probe)

Two variants must not be conflated:

* **Per-txn exact allocation** — the txn advances a counter row by exactly the rows it
  inserts (`UPDATE counter SET val = val + K RETURNING val` inside the txn). Burns
  **zero** on rollback *and* on crash (MVCC abort; verified via `pg_terminate_backend`:
  counter unchanged, nothing committed, no reuse). Cost: one serialized counter UPDATE
  per txn — measured **3.5-4.4x slower** than column-DEFAULT `nextval` at K=1 (~290 vs
  ~1030-1270 tps, p95 ~60 ms vs ~7-11 ms, 8 clients x 100 single-row commits).
* **Per-writer cached blocks** — a writer reserves a block of N once, numbers rows from
  a local cache across txns. Contention tracks `nextval` within noise (cached 32-blocks
  ~837-1059 tps). Crash burns the unused remainder (verified: 64-block, 5 used, killed
  holder → 59 values permanently burned, next writer got 66).

**Critical hazard for the cached-block variant:** the gap fence's proof
(`snap.xmin >= fence_xmax`, `subscriber_state.rs:350`) rests on the premise that a
missing seq's writer, if any, was already in-flight (held an xid) at detection. That is
true under `nextval` (the inserting txn consumes the value itself) and true under
per-txn allocation (the counter UPDATE is inside the inserting txn), but **false under
cached blocks**, where the future filler of a missing seq may have no xid yet — a
fence-"proven" gap could later materialize rows from below. Choosing the cached-block
variant would require reworking the fence semantics, not just the allocator.

Ordering caveat (observation only): all bus reads are `global_sequence`-ordered
cursors, and under batched allocation seq order ≠ commit order across writers.

`epoch_mem` has no sequence numbers at all (`Event.global_sequence: Option<u64>`,
`epoch_core/src/event.rs:66-73`), so option B imposes no mem-backend parity work.

### Prior-art constraints (CLOUD-227 / CLOUD-232)

* Spec 0026 (`specs/0026-cloud226-contiguous-catchup-checkpoint.md`) proved "The fence
  proves the writer finished, not aborted" (§3). Option A deliberately inverts this for
  opt-in subscribers: it advances past a gap the fence could not prove.
* Spec 0027 (`specs/0027-cloud232-live-path-contiguous-checkpoint.md`) pinned: a
  persisted checkpoint "MUST NEVER lead `state.contiguous_checkpoint`" (§3.3). Any
  SkipAfterBackstop HWM-advance must respect this or amend it explicitly for the
  opt-in class.

## Constraints

* Project conventions (`AGENTS.md`): spec-first, TDD, `cargo fmt`/clippy clean,
  rustdoc on public APIs, Conventional Commits, integration tests parallel-safe.
* catacloud carries ~530 lines of generation-chain heal (CLOUD-259 Phase 3, commit
  `614c06ae`) as the current workaround; either option makes it unnecessary for the
  burn class. Its safety depends on wedge-inertness invariants that the sibling
  unsubscribe/P5 ticket would change.
* The two existing fail-closed suite tests
  (`test_fail_closed_gap_refusal_and_fence_cleared_recovery`,
  `test_wedged_gap_does_not_starve_peer_and_release_resumes`,
  `epoch_pg/tests/pgeventbus_integration_tests.rs:8432-8580`, `:8718-8880`) pin current
  semantics; changes here are behavior-visible and need explicit spec treatment.

## Decisions the spec must frame (with a recommendation, not a hedge)

1. Option A, option B, or both. The ticket allows any of the three.
2. If B: per-txn exact (fence-safe, 3.5-4.4x per-txn cost at K=1) vs cached blocks
   (fast, but breaks the fence premise and would drag fence rework into scope).
3. If A: policy opt-in shape (`Halt` default, `SkipAfterBackstop` per subscriber),
   skipped-seq tracking and late-row detection mechanism, rebuild trigger semantics,
   and whether the wedge path (not just fail-open skips) starts recording
   `epoch_event_bus_gap_timeouts` rows for audit.
4. Whether to also fix the adjacent defects surfaced by the probes (release_halt
   over-promising WARN for ReplayAlways; catch-up/live double-delivery after fresh
   subscribe over a hole) or ticket them separately.
