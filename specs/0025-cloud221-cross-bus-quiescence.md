# Spec 0025: `epoch_pg` cross-bus quiescence — unresolved design problem

**Issue:** Linear CLOUD-221 (R3, split out of spec 0024)
**Title:** epoch: cross-bus fixed-point quiescence gate for saga-driven cascades
**Status:** Design problem — **not** ready for a delivery plan (see §7 Open Decision)
**Created:** 2026-08-18
**Crates:** `epoch_pg` (a cross-bus readiness gate; the listener concurrency model it
would depend on lives in `epoch_pg/src/event_bus/`)
**Commit scope:** `feat(pg)` / concept scope `event-bus` (when eventually delivered)
**Workspace version:** `0.1.0` (pre-1.0)
**Depends on:** **Spec 0024 (CLOUD-221 R1/R2/R4/R5)** — prerequisite. 0024 ships the
mechanism (`ReplayAlways` + per-subscriber HWM readiness + pre-loop catch-up + trigger
safety); this spec is the cross-bus gate that must sit on top of it before catacloud can
delete its raw-SQL `PolicyGraph` warm-up.
**Forcing consumer:** catacloud `integration/src/lib.rs::initialize_aggregates` (nine
`PgEventBus` instances + the raw-SQL `PolicyGraph` warm-up block, `~1785-1822`). The
warm-up deletion is **blocked on this spec**, not on 0024 alone (§1, §2).
**Migrations:** none anticipated.

---

## 0. Why this spec exists separately

Spec 0024 originally carried five requirements. R3 — a cross-bus fixed-point quiescence
wait exposed as `wait_until_quiescent`, backed by a `ReadinessProbe` trait with a
`fillable_head()` method and a `QuiescenceReport` — was found **unimplementable as
specified** during review. The defect is not a coding detail: the specified API cannot be
implemented correctly on the current listener, and making it implementable requires a
change to the listener's **concurrency model** that none of the other four requirements
need.

The decision was therefore to **split**: 0024 keeps R1, R2, R4, R5 and ships now; R3
moves here as an **unresolved design problem** to be settled before it gets a delivery
plan. This file records the requirement, the full blocker analysis (verified against
source), the candidate options with their trade-offs, and the open decision that gates any
implementation. It deliberately contains **no phased-delivery JSON block**: there is no
delivery plan until the open decision in §7 is resolved.

---

## 1. Problem Statement — the requirement

catacloud boots nine `PgEventBus` instances and then warms up its in-memory `PolicyGraph`
by scanning `policy_events` with raw SQL (`integration/src/lib.rs:~1785-1822`), applied
directly to the shared graph handle, bypassing the checkpoint system. Spec 0024 lets that
projection be expressed as a normal `ReplayAlways` `subscribe()` with a per-subscriber
readiness gate (`wait_until_caught_up`). But a **per-subscriber** gate is not enough to
delete the warm-up safely, because the completeness of the policy graph depends on a
**saga-driven, cross-bus cascade** having drained first.

The CLOUD-217 incident chain is cross-bus:

```
iam_bus / compute_bus --(bootstrap events)--> PolicyLifecycleSaga
PolicyLifecycleSaga    --(policy commands)---> policy_events rows
policy_events          --(new rows)----------> PolicyGraphProjection on policy_bus
```

Checking `policy_bus` in isolation can report "caught up to head" **before** the saga on
`iam_bus` / `compute_bus` has dispatched the commands that would extend `policy_events`'s
head: a **false ready** that reproduces the incident through the new API. A subscriber
parked at the current `policy_bus` head satisfies `wait_until_caught_up`, the process
starts serving, and the policy graph is silently missing every row the still-running
cascade was about to produce. This is exactly the failure class 0024's local gate cannot
close, and precisely why deleting the warm-up behind a per-subscriber gate is **unsafe**.

**The requirement (R3):** a startup gate that verifies the *whole* saga-driven cascade
across all relevant buses has drained — a cross-bus readiness check — **without** the
consumer having to declare a bus dependency graph.

### Goal / Non-Goals

- **G-1** Provide a cross-bus readiness gate that a consumer can use as a startup barrier,
  so that a saga-driven cascade (`iam/compute → policy_events → policy_bus`) is known to
  have drained before traffic is served, with no consumer-declared dependency graph.
- **NG-1** Not a per-subscriber gate. 0024's `wait_until_caught_up` is a **local** check
  and is documented there as unsafe for this purpose; this spec is what makes the warm-up
  deletion safe.
- **NG-2** No new persisted state / no migration is anticipated.
- **NG-3** No horizontal-scaling / multi-instance readiness (see P1 and §5 on Coordinated
  mode).

---

## 2. Why a per-subscriber gate is necessary but not sufficient

A per-subscriber gate (0024's `wait_until_caught_up`) is **necessary**: each subscriber
must reach the head that exists once the cascade settles. It is **not sufficient**,
because at any instant during the cascade the downstream head has not yet been extended by
the still-running upstream saga, so "caught up to the current head" is a moving target
that a single-subscriber, single-head-snapshot wait cannot chase safely. The original R3
proposed a **fixed point over all supplied buses**: repeat full passes over every
registered subscriber on every supplied bus until a complete pass observes no advancement
anywhere (no head grew, no subscriber position moved). The termination argument: while a
saga is still dispatching downstream commands, the downstream bus's head grows on the next
snapshot, so the "no advancement" condition fails and the loop continues; only when the
whole cascade has drained can a full pass see zero movement. This is robust to cascade
depth and needs no `iam→policy` dependency declaration from the consumer.

That shape is correct at the level of *intent*. The problem is the **predicate it needs to
evaluate** and the **listener state it needs to read** — both detailed in §4.

---

## 3. Codebase grounding (verified 2026-08-18)

All anchors below were re-checked against source while splitting this spec out; they are
the load-bearing facts for the blocker analysis in §4.

| Fact | Location | Relevance |
|------|----------|-----------|
| The gap fence is **per-subscriber**: `SubscriberState.gap_first_seen: HashMap<u64, GapObservation>`, each `GapObservation` carrying `first_seen` and a `fence_xmax` **captured when that subscriber first saw the gap** | `epoch_pg/src/event_bus/subscriber_state.rs:64-73` (`GapObservation`), `:100-118` (`SubscriberState` fields) | The permanence decision is stateful and per-subscriber; it cannot be reconstructed from bus-global data |
| Permanence proof is `snap.xmin >= fence_xmax` using the **historical** `fence_xmax` (captured at first observation), not a fresh `xmax` | `subscriber_state.rs:194-205` (fence fast-path in `advance_contiguous_checkpoint`) | A fresh snapshot cannot prove permanence; only the historical fence can |
| `PgEventBus`'s fields are exactly `pool, channel_name, projections, config, listener_state, inline_state` — **no fence state** | `epoch_pg/src/event_bus/mod.rs:665-677` | A bus-level method has nothing to read: fence state does not live on the bus |
| The `HashMap<String, SubscriberState>` is a **stack local** inside the listener task | `mod.rs:~974` | Per-subscriber fence state is not reachable from any `&self` method |
| Backstop advances the **subscriber's** contiguous checkpoint past a burned gap via `SkipReason::TimeoutBackstop`; it does **not** move raw head and is not observable as a bus scalar | `subscriber_state.rs:223-238` | Convergence is a subscriber-scope fact, not a bus-scope one |
| Listener uses a **move-out / move-back** ownership model: `subscriber_states.remove(sid)` moves each state by value into a concurrent per-subscriber task, `join_all(...).await` runs them, `subscriber_states.insert(outcome.subscriber_id, outcome.state)` moves them back | `mod.rs:~1261` (remove), `:~1284-1290` (`join_all`), `:~1308` (insert) | During batch processing, the state of every actively-working subscriber is **absent** from the map |
| Subscribers are deduplicated by id via a `seen_sids` set, giving compile-time exclusivity alongside the move semantics | `mod.rs:~1259` | The current model's safety rests on move semantics + dedupe, not on runtime locks |
| The incident saga dispatches **synchronously**: `PolicyLifecycleSaga::handle_event` calls `dispatch_all(...).await` inline, no `spawn` | catacloud `integration/src/sagas/policy_lifecycle.rs:46,223` | P2 (§6) holds today for the forcing consumer, but epoch cannot enforce it |

---

## 4. The blocker — why the specified R3 is unimplementable

The original R3 specified `fillable_head()` as a **bus-level** method on a `ReadinessProbe`
trait, and defined quiescence as: no head grew, no subscriber position moved, **and** every
subscriber position equals its probe's `fillable_head`. The last clause is where it breaks.

### 4.1 `fillable_head()` was specified bus-level, but the fence it needs is per-subscriber and historical

The "highest fillable sequence" is not a property of the bus. It is a property of a
**subscriber's** gap-fence state:

- Each gap a subscriber holds carries a `fence_xmax` **captured when that subscriber first
  observed the gap** (`GapObservation { first_seen, fence_xmax }`,
  `subscriber_state.rs:64-73`, populated in `advance_contiguous_checkpoint`).
- The proof that the gap is permanent (so the subscriber may advance past it) is
  `snap.xmin >= fence_xmax` using that **historical** `fence_xmax`
  (`subscriber_state.rs:194-205`).
- `PgEventBus`'s fields are only `pool, channel_name, projections, config, listener_state,
  inline_state` (`mod.rs:665-677`): there is **no fence state on the bus**.
- The `HashMap<String, SubscriberState>` that holds the fences is a **stack local** inside
  the listener task (`mod.rs:~974`).

So a bus-level `fillable_head()` cannot reach the per-subscriber historical `fence_xmax`,
and **one scalar cannot be correct for two subscribers on the same bus holding different
gaps**. The method as specified has no correct implementation.

### 4.2 Every bus-scope implementation lands on one horn of a dilemma

Because the fence is per-subscriber and historical, any attempt to compute a single
bus-scope `fillable_head` must approximate, and every approximation is wrong in one of two
directions:

- **Over-report unfillability** (e.g. "highest contiguous visible run below head"):
  suppose `nextval` burns sequence 4 for an **uncommitted** transaction while sequence 5 is
  already visible. This heuristic reports `fillable_head = 3`. A subscriber parked at 3 then
  satisfies the predicate, the gate **settles while event 4 is still going to commit**, and
  the consumer starts with a graph missing event 4. That is a **false ready** — the incident
  itself. Note that precondition P1 (no concurrent external appends, §6) does **not** close
  this: P1 forbids *new* appends, but a transaction already **in flight at gate start** can
  commit during the gate.
- **Under-report** (only exclude a gap when a **freshly captured** snapshot proves
  permanence): a fresh snapshot has `fence_xmax = xmax_now` and `xmin < xmax` on first
  observation, so the fence never clears immediately. The tail is therefore always held as
  "still to fill", the gate can never settle until its own `gap_timeout` fires, and the
  feature's whole point — settling **without** waiting out `gap_timeout` — is defeated.

There is no middle setting: the correct answer depends on the *historical* fence each
subscriber captured, which a bus-scope method cannot see.

### 4.3 The convergence claim is true only at subscriber scope

The original §7.2 convergence argument ("a fenced tail eventually stops wedging the gate")
is real, but it lives at **subscriber** scope: the `SkipReason::TimeoutBackstop` path
(`subscriber_state.rs:223-238`) advances the **subscriber's** `contiguous_checkpoint` past a
burned gap. It does **not** move raw head, and it is **not observable as a bus scalar**. So
the very mechanism that makes quiescence eventually converge is invisible to a bus-level
probe.

### 4.4 The reviewer's proposed replacement predicate

Drop `fillable_head()` entirely. Key quiescence on **per-subscriber checkpoint stability**
instead of a bus scalar: expose per subscriber `(position, has_unresolved_gap_below_head)`,
and define *settled* as:

> no subscriber position moved across a full pass **AND** no subscriber holds an unfenced,
> pre-backstop gap.

This keeps the fence decision where the state actually lives (the subscriber) and avoids the
impossible bus-scalar collapse.

### 4.5 Why the replacement is not yet approved — the listener ownership problem

The replacement predicate needs to **read every subscriber's live state** (its position and
whether it holds an unfenced, pre-backstop gap below head) while the listener is running.
The current listener makes that read unsafe:

- The listener uses a **move-out / move-back** ownership model. Each subscriber's
  `SubscriberState` is moved **by value** out of the map into a concurrent per-subscriber
  task: `subscriber_states.remove(sid)` (`mod.rs:~1261`), the tasks run under
  `join_all(...).await` (`:~1284-1290`), and the states are moved back with
  `subscriber_states.insert(outcome.subscriber_id, outcome.state)` (`:~1308`).
- Therefore, **during batch processing, the state of every actively-working subscriber is
  absent from the map.** A probe that locked and read the map would find **no entry exactly
  for the busy subscribers** — the ones whose readiness the gate most needs to observe.

Merely wrapping the map in a shared lock does not expose the state; it exposes an empty slot.
The two ways out both cost something:

- **(a) Shadow copy** updated on merge: a second map the listener writes on each
  merge-back. It is **stale by up to a batch**, and stale-low is the **unsafe** direction —
  the gate could settle while a batch is still in flight (the position it reads is behind the
  true position, so "no movement" can be observed spuriously). Reporting stale-**high** would
  be safe-but-slow; a merge-back shadow is stale-**low**.
- **(b) Per-subscriber `Arc<Mutex<SubscriberState>>`** mutated in place: this converts a
  **compile-time** exclusivity guarantee (move semantics plus the `seen_sids` dedupe at
  `mod.rs:~1259`) into a **runtime lock discipline** inside a function that `await`s I/O,
  with a probe polling (e.g. every 25 ms) across N subscribers × 9 buses. That is a real
  concurrency-model change to the hot path, with deadlock/contention surface the current
  design does not have.

Both routes are a change to the **listener's concurrency model**. None of 0024's other four
requirements touch it. That is the reason this is a separate, unresolved spec rather than a
line-item fix inside 0024.

---

## 5. Options and trade-offs

| Option | Shape | Trade-off |
|--------|-------|-----------|
| **A. Per-subscriber-quiescence (reviewer's design) via shadow copy** | Replace `fillable_head` with per-subscriber `(position, has_unresolved_gap_below_head)`; the listener maintains a shadow map, updated on merge-back, that a probe reads under a lock (§4.4, §4.5a) | Correct predicate; no bus-scalar collapse. **But** the shadow is stale by up to a batch, and stale-**low** is the unsafe direction (can false-settle mid-batch). Would need the shadow written **before** the move-out, or a safe-high bias, to be sound. |
| **B. Per-subscriber-quiescence via `Arc<Mutex<SubscriberState>>`** | Same predicate; state mutated in place under a per-subscriber lock so a probe reads the live value (§4.5b) | Reads are always current. **But** trades the current compile-time exclusivity (move + `seen_sids` dedupe) for runtime lock discipline in an `await`-heavy hot path, polled across N×9 subscribers. New deadlock/contention surface on the critical delivery path. |
| **C. Ship raw-head quiescence with `!settled` as a warning, not a refusal** | Keep the fixed-point-over-buses shape, compare positions against **raw head**, and treat a non-settling result as a **logged warning** rather than a hard startup refusal | Cheapest; no listener change. **But** under a burned/fenced tail a genuinely idle system reports `!settled` until `gap_timeout`, so the signal is noisy, and downgrading it to a warning means it no longer *gates* — the consumer can still start stale. Does not, by itself, make the warm-up deletion safe. |
| **D. Something better** | e.g. expose a purpose-built, safe-high per-subscriber readiness snapshot the listener publishes at a well-defined point (batch boundary) with an explicit staleness/direction contract, so a probe never reads stale-low | Open. The point of this spec is that the right answer here is **not yet chosen**. |

The recurring axis is **safety direction under staleness**: any snapshot a probe reads must
be biased so that its error can only make the gate *refuse* (safe) and never *settle early*
(unsafe). Options A and C fail that today; B pays for currency with a hot-path lock.

---

## 6. Preconditions (carried from the original R3; still required by any option)

- **P1 — no concurrent external appends to any probed bus for the gate's duration.** The
  fixed point can never settle while anything else appends: an ordinary background writer
  makes the "no head grew" condition fail on every pass, so the gate always times out.
  catacloud has exactly such writers (orphan-machine and abandoned-job checkers on 60 s
  timers), and `InstanceMode::Coordinated` explicitly permits **peer instances** to write.
  The consumer must therefore start those background tasks **after** the gate returns and,
  under Coordinated mode, run the gate only where no peer is writing. epoch cannot detect
  peer writes; P1 is a caller-guaranteed precondition, not a formality. **Note:** P1 forbids
  *new* appends but does **not** cover a transaction already in flight at gate start — see the
  over-report horn in §4.2.
- **P2 — a saga appends its downstream event durably before its own monitored position
  advances past the trigger.** The termination argument silently assumes this. It holds for
  the forcing consumer today: the listener advances a subscriber's checkpoint only after
  `process_event_with_retry` succeeds, `SagaHandler::on_event` awaits `process_event`, and
  `PolicyLifecycleSaga::handle_event` dispatches synchronously (`dispatch_all(...).await`
  inline, no `spawn`; catacloud `integration/src/sagas/policy_lifecycle.rs:46,223`). But if
  any saga ever dispatches **asynchronously** (spawn / queue / fire-and-forget), a window
  opens where the saga has consumed event N, its position `== head`, and the downstream
  command is in flight but appended nowhere: every quiescence condition is satisfied and the
  gate **false-settles**, reproducing the incident. This is a **cross-repo precondition epoch
  cannot enforce**; it must be documented on whatever gate this spec eventually ships.

Both preconditions exist to serve the cross-bus gate specifically, which is why they live
here and not in 0024.

---

## 7. Open Decision (blocks any delivery plan)

**The concurrency-model question must be settled before this spec gets a delivery plan.**
Concretely, one of these must be decided and agreed:

1. **Which predicate** the gate evaluates: the reviewer's per-subscriber
   `(position, has_unresolved_gap_below_head)` (§4.4), or a different formulation that avoids
   reading live per-subscriber fence state.
2. **How the listener exposes per-subscriber readiness** to a probe **without a stale-low
   read**: shadow copy written safe-high at a batch boundary (Option A, made sound),
   in-place `Arc<Mutex<_>>` on the hot path (Option B), or a purpose-built snapshot with an
   explicit staleness/direction contract (Option D).
3. **Whether the cross-bus gate hard-refuses startup or only warns** when it cannot settle
   (Option C), given that a warning does not make the warm-up deletion safe.

Until (1)–(3) are resolved, there is **no phased-delivery plan** and **no machine-readable
phases block** in this spec: writing one would imply a design that has not been chosen. This
is deliberate.

---

## 8. Relationship to spec 0024

- **0024 is a prerequisite.** It delivers the *mechanism*: `ReplayAlways` replay-from-zero,
  per-subscriber in-memory HWM readiness, the pre-loop catch-up pass, and trigger safety.
  Its `wait_until_caught_up` is a **local** readiness check, documented there as unsafe as a
  startup gate for any consumer whose sagas cascade across buses.
- **This spec is the missing safe gate.** catacloud's raw-SQL `PolicyGraph` warm-up
  (`integration/src/lib.rs:~1785-1822`) must **not** be deleted behind 0024's per-subscriber
  gate: the policy graph's completeness depends on the `iam/compute → policy_events` saga
  cascade having drained, which only a cross-bus gate can verify. The warm-up deletion is
  therefore **blocked on this spec**, and 0024's acceptance criteria say so.
- When the §7 open decision is resolved, this spec gains a §Design (chosen option), a
  §Requirements table, a §Test Plan (including the integrated incident-shape test — a
  `ReplayAlways` projection on `policy_bus` gated by the cross-bus gate while an upstream
  saga-surrogate extends `policy_events`, asserting the model is complete exactly when the
  gate settles), and a phased-delivery plan with a machine-readable block. Not before.
