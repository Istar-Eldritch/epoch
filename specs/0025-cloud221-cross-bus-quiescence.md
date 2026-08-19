# Spec 0025: `epoch_pg` cross-bus quiescence — unresolved design problem

**Issue:** Linear CLOUD-221 (R3, split out of spec 0024)
**Title:** epoch: cross-bus fixed-point quiescence gate for saga-driven cascades
**Status:** Design problem — **not** ready for a delivery plan (see §7 Open Decision)
**Created:** 2026-08-18
**Last probed:** 2026-08-19 — three independent read-only investigations re-verified this
spec against current `main` (post-0024) and against catacloud. Their findings are in §9,
and they **materially change the conclusion**: read §9 before acting on §4–§7, some of
which is now known to be stale or overstated.
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
warm-up deletion is **blocked on this spec**, not on 0024 alone (§1, §2). **Superseded by
§9.4:** the policy warm-up appears to be deletable *today* by ordering two existing
`wait_until_caught_up` calls, with no new epoch code. The general gate is no longer on this
consumer's critical path.
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

---

## 9. Probe findings (2026-08-19) — what changed

Three independent read-only probes re-verified this spec against `epoch` @ `main` (with 0024
merged) and against catacloud. Everything below was checked against source; line numbers are
current as of 2026-08-19. Where a probe contradicted the spec, the contradiction wins and is
marked. **§4–§7 above are preserved as the original argument, but §9 supersedes them where
they conflict.**

### 9.1 §3's anchors are stale; one entry is now false

0024 rewrote ~1090 lines of `event_bus/mod.rs`, so every `mod.rs` line number in §3 has
shifted. The **claims** mostly survive; the **citations** did not. Corrected:

| §3 fact | Current anchor | Verdict |
|---|---|---|
| `GapObservation { first_seen, fence_xmax }` | `subscriber_state.rs:67-73`; `SubscriberState` `:89-104` (`gap_first_seen` `:103`) | stale line, claim true |
| Fence proof `snap.xmin >= fence_xmax` | `subscriber_state.rs:199-200` (block `:196-211`) | stale line, claim true |
| `PgEventBus` has no fence state | struct `mod.rs:726-743` | **NOW FALSE as written.** 0024 added a **7th** field, `hwm: Arc<Mutex<HashMap<String,u64>>>` (`:742`), alongside `pool`(730) `channel_name`(731) `projections`(732) `config`(733) `listener_state`(735) `inline_state`(737). The *conclusion* holds (`hwm` is not fence state) but the "fields are exactly …" phrasing is wrong, and `hwm` is precisely the class of bus-reachable per-subscriber state §4.1 asserts cannot exist. |
| `subscriber_states` is a listener stack local | `mod.rs:1135`, inside the `tokio::spawn` at `:1045` | stale line, claim true |
| Backstop advances subscriber checkpoint only | `subscriber_state.rs:220-236` | stale line, claim true |
| Move-out / move-back | `.remove(sid)` `:1435`, `join_all` `:1459`, merge-back `.insert` `:1482` | stale line, claim true — **the core blocker is real and current** |
| `seen_sids` dedupe | `mod.rs:1429` | stale line, claim true |

### 9.2 The blocker is narrower than §4.5 claims — position is already exposed

§4.5 treats "expose per-subscriber readiness" as one monolithic problem requiring a
concurrency-model change. It is **two** problems, and one is already solved:

- **Position needs no new plumbing.** It is already readable from `&self` while the listener
  runs: `get_checkpoint()` (`mod.rs:1563`) for `Checkpointed`, the `hwm` map for
  `ReplayAlways`, and `head_sequence()` (`:1619`) for the target. All three have the **safe**
  error direction (under-report ⇒ bias toward refusing).
- **Only one bit is genuinely missing:** the per-subscriber gap flag
  (`!gap_first_seen.is_empty()`) — a single boolean, not the whole `SubscriberState`. Option A
  therefore does not require a shadow of the full state, and Option B is unnecessary.
- **The two halves have opposite safe directions.** For position, safe = under-report. For the
  gap flag, safe = **over-report** ("assume gap present until proven absent"). A single "busy"
  boolean cannot express both, which the §5 framing misses.

Refinements to §4.5's mechanics: the move-out is **per-priority-group**, not
all-subscribers-at-once (groups are processed in ascending priority, each fully merged back
before the next removes anything), and it repeats per page while `batch_was_full`. Also,
`subscriber_states` is a **bare local `HashMap` behind no lock at all** today, so §4.5's "a
probe that locked and read the map" describes a hypothetical wrapper, not existing code.

**Incidental defect found, independent of this spec:** `projections.lock()` is held at
`mod.rs:1282` across the *entire* backlog-drain batch loop, so anything wired through that
lock blocks for the whole drain. This is the same defect class 0024 just fixed for the R2
catch-up pass (snapshot-then-release, `:1076-1100`). Any future probe must not wire through
`projections`.

### 9.3 Option D specified, Option E measured

- **D-1 (recommended): listener-published quiescence channel.** Add one field to
  `PgEventBus`: a `tokio::sync::watch::Sender<BusQuiescence>` where `BusQuiescence
  { generation: u64, settled: bool, per_subscriber: HashMap<String,(u64,bool)> }`. The
  listener publishes at exactly two points: `settled=false` immediately on waking from
  `select!` (`~:1211`), before touching any state; and a fresh snapshot at the bottom of the
  outer loop once the inner batch loop exhausts (`rows.is_empty()`, `:1361`), where
  `subscriber_states` is fully merged back. **Direction contract:** a reader can only observe
  state that was true while the listener is asleep, or `settled=false`. Any observed
  `settled=false` — however fleeting — resets a prober's consecutive-settled counter, which is
  what kills the stale-low race in Option A. **Blast radius: one field, two publish points, no
  change to move-out/move-back and no new hot-path lock.** This also inverts the design
  usefully: the listener already owns the state, so nothing needs to read it out.
- **D-2: durable checkpoint + activity heartbeat.** Works cross-process, but (a) under
  `CheckpointMode::Batched` the persisted row lags in-memory position — the *unsafe*
  direction — and (b) `ReplayAlways` subscribers have **no persisted checkpoint by design**,
  so it cannot observe the one subscriber that matters, without new persisted state
  (contradicting NG-2). Ranked below D-1.
- **D-3: shrink `gap_timeout` during the gate.** Dead. It only makes `TimeoutBackstop` (which
  the code itself documents as potential data loss) fire sooner: trades correctness for
  latency, the wrong way relative to the safety invariant.
- **E (xmin fence): viable, narrow, and not new.** Measured against the project's own
  Postgres 14 container. `pg_current_snapshot()`'s `xmax` is `latestCompletedXid + 1`, so
  while a write transaction is live, `xmin == xmax == that xid` and stays frozen for its whole
  duration; rollback releases exactly as fast as commit; read-only
  `idle-in-transaction` sessions do **not** pin `xmin` (only a real assigned xid does). This
  is why the existing inclusive `xmin >= fence_xmax` test is correct, which means **Option E
  is the existing per-gap fence relocated to gate scope, not new engineering** (`query_txid_snapshot`,
  `mod.rs:161-183`). **New hazard, empirically confirmed and not anticipated anywhere above:
  `xmin` is cluster-wide.** Any open write transaction anywhere in the Postgres instance — an
  unrelated batch job, a forgotten uncommitted `INSERT` in a debug session — pins global
  `xmin` and stalls the gate indefinitely even when every probed bus is quiescent. That is
  strictly broader than P1, and it forces the same timeout-backstop escape hatch, which
  reintroduces §7.3's hard-refuse-vs-warn tension. **Scope limit:** E closes only §4.2's
  "transaction already in flight at gate start" horn. It does not address position tracking
  and does not replace D-1.

### 9.4 The forcing consumer may not need this spec at all

Two probes independently concluded the premise is weaker than stated.

**A pre-existing mechanism this spec never mentions:** `subscribe()` performs its **own
fully synchronous, gap-free catch-up** before its future resolves — it awaits
`catch_up_from_checkpoint(...)` and then drains a buffered `PgListener` channel
(`mod.rs:~2973-2995`), entirely distinct from 0024's R2 pre-loop pass. Verified directly.

**Consequence:** because every catacloud saga dispatches synchronously (see 9.5), a saga's
checkpoint advances only *after* every downstream command it triggered has been dispatched
and committed. So the ordering below is sufficient for the policy cascade, using only shipped
0024 API and **zero new epoch code**:

1. `iam_bus.wait_until_caught_up("policy_lifecycle_saga", timeout)` → `true`
2. *then* `policy_bus.wait_until_caught_up("policy_graph_projection", timeout)`

Step 2 snapshots its target head *after* step 1 proved the upstream drained, so there is no
moving-target race — the ordering removes it rather than chasing it. This trades away NG-1
(catacloud must declare "policy_bus depends on this saga"), but catacloud already owns that
saga, so the knowledge costs nothing new.

**Also verified:** `start_listener()` **spawns** its R2 catch-up and returns without awaiting
it (`:1045` spawn, `~:1491` return). Today's catacloud warm-up is therefore safe against the
original CLOUD-217 ordering race, but *not* for the reason its own code comment gives
(`catacloud integration/src/lib.rs:1767-1780`) — the real mechanism is `subscribe()`'s
synchronous catch-up, called earlier. Anyone editing that file trusting the comment could
reintroduce the race.

### 9.5 P1 and P2 verified; NG-1 partly vindicated by a real cycle

- **P2 (synchronous dispatch): holds repo-wide.** All ~40 catacloud sagas were checked: zero
  use `tokio::spawn`, unbounded channels, or fire-and-forget. Confirmed at
  `integration/src/sagas/policy_lifecycle.rs:46,223`.
- **P1 (no concurrent writers): holds, more strongly than §6 credits.** Nine-plus periodic
  writers exist (`stale_machine_checker`, `job_timeout_checker`, credit/bundle/storage/
  subscription checkers, …) but every `spawn_*_checker` sits at
  `web-admin/src/bin/catacloud_web.rs:852-942`, strictly after `initialize_aggregates()`
  (`:644`), and `HttpServer::bind()` (`:986-987`) comes after those.
- **Coordinated mode is moot today.** `InstanceMode` is never referenced in catacloud; every
  bus uses `..Default::default()`, and the default is `SingleInstance`. It becomes a live risk
  only if catacloud is horizontally scaled, and nothing would detect peer writes then.
- **NG-1 survives as a general constraint, because a real cycle exists.** The
  organization-deletion cascade runs `iam_bus` → {`files`, `annotations`, `jobs`, `compute`,
  `billing`} → back to `iam_bus` (via `IamCleanupPort::acknowledge_cleanup`, implemented by
  `OrganizationAggregate`, `iam/src/port.rs:29`, then
  `iam/src/sagas/organization_deletion_finalizer.rs:14`) — a genuine cycle through 6 of the 9
  buses. Topological sequencing (9.4) therefore works for the policy case but **cannot**
  generalize. Automatic derivation of the graph from saga registrations is not cheap either:
  `SagaAdapter::new` declares the source bus, but the destination bus is implicit in whichever
  port the saga holds, with no registry linking them.

### 9.6 A cross-repo blocker no gate design can fix

`PolicyLifecycleSagaError` is **uninhabited** (`enum PolicyLifecycleSagaError {}`,
`catacloud integration/src/sagas/policy_lifecycle.rs:204`) and `dispatch_all` (`:46-51`) logs
and swallows every dispatch error. `handle_event` therefore always returns `Ok`,
`SagaHandler::on_event` never sees a failure, and the listener advances the checkpoint
regardless. **If `policy_port.dispatch` fails even transiently, the saga's checkpoint still
reaches head, so every readiness gate — 0024's or any 0025 design — correctly reports "caught
up" while the policy command was permanently lost.**

The method's own docstring justifies this by saying "a partial failure followed by a bus retry
is safe", but because the error never propagates there **is no retry**: the justification
depends on a mechanism the code disables. This is a permanent silent-loss bug, not a race, and
it is unique to this saga (every other catacloud saga propagates real errors). **It is
strictly higher priority than this spec**: deleting the warm-up behind any gate while this
stands would trade a working belt-and-braces for a gate that cannot detect the actual failure
mode. Fix belongs in catacloud, not epoch.

### 9.7 Revised open decision

§7's three questions are now largely answerable:

1. **Predicate** — per-subscriber `(position, has_unresolved_gap_below_head)` stands, with the
   opposite-safe-directions correction from 9.2.
2. **Exposure** — **D-1**, hardened with **E** if the in-flight-at-gate-start horn matters.
   Runner-up B, rejected: it buys always-current reads with a real hot-path lock rewrite when
   D-1 gets equivalent safety from a busy/idle tri-state.
3. **Refuse vs warn** — still genuinely open, and now sharper, because E's cluster-wide `xmin`
   stall (9.3) and the backstop escape hatch mean a hard refusal can wedge startup on
   something entirely outside the probed buses.

**What should happen first, in order:** (a) fix the catacloud swallow bug (9.6); (b) delete the
warm-up behind the two ordered `wait_until_caught_up` calls (9.4) and keep the existing
CLOUD-217 regression test as the guard; (c) build D-1 only when a consumer actually needs a
cascade gate with no declarable order — the cyclic org-deletion cascade (9.5) is the honest
candidate, and it is not what this spec was written for. Steps (a) and (b) need no epoch
change at all, which means **this spec is no longer blocking its own forcing consumer.**

Still unknown / not probed: the worst-case duration of the move-out window (bounded by
`process_event_with_retry`'s retry-and-backoff behaviour, unmeasured); whether
`unwrap_or_else(SubscriberState::new(0))` at `mod.rs:1436` is reachable at all (suspected dead,
unconfirmed); and empirical validation of the 9.4 ordering claim against a running catacloud
(the existing regression test at
`catacloud integration/tests/dao_integration/dao_integration_policy_graph_warmup_ordering.rs`
was read, not executed).
