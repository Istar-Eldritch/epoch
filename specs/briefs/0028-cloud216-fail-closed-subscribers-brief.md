# Brief: CLOUD-216, per-subscription fail-closed delivery semantics

Input for the spec-writer. This is a technical brief, not a spec: it states the problem, the
verified mechanism, what changed since the ticket was filed, the design direction discovery
converged on, and the open questions the spec must settle. Everything marked VERIFIED was read in
source on `main` at `a19d001`; everything marked INFERRED still needs proving.

## 1. Problem

`epoch_pg`'s Postgres event bus is unconditionally **fail-open**: on every failure path it logs,
skips, and **advances the checkpoint past the event anyway**. There is no way for a subscriber to
opt into the opposite contract — *"if I cannot correctly apply every event in order, do not
silently move on"*.

The motivating consumer is `catacloud`'s `PolicyGraphProjection`: the in-memory authorization
graph read on every `policy.evaluate()`/`scope()` call. A silently skipped revocation event is a
silent over-grant. CLOUD-216 (filed 2026-07-30) framed this as a design pass **in preparation
for** moving that projection onto the bus: at filing time the projection bypassed epoch's
subscribe/checkpoint machinery entirely (raw full-replay per boot, abort on decode failure) and
the ticket was a prerequisite for the snapshot/warmup optimization.

### What changed since filing: the consumer already moved onto the bus

CLOUD-217 (catacloud commit `b089a89f`, "replace raw-SQL policy-graph warm-up with ordered
readiness gate") migrated `PolicyGraphProjection` onto the standard bus path. On current
catacloud `main`:

- `integration/src/lib.rs:1444` — `policy_bus.subscribe(ProjectionHandler::new(policy_graph_projection))`.
  Hydration now runs through `subscribe()`'s catch-up pass; live updates through the listener.
- The old invariant — *"An authorization oracle rebuilt from a stream with holes must not come
  up. Fail startup loudly"* — is **deleted, not relocated**: no grep hit remains anywhere in
  catacloud. A TestApp-only `else` branch still replays pre-existing history via `SELECT`
  (`a03e6054`), with `.expect()` on decode — but that path is test-only.
- The production authorization oracle therefore currently runs on **fail-open semantics**: a
  `PolicyEvent` row that fails to deserialize is WARN-skipped and checkpointed past, on every
  path, forever. The risk CLOUD-216 described has graduated from "future consumer blocked" to
  "current consumer unprotected".

Two mitigating facts (VERIFIED, catacloud `policy/src/dao/graph_projection.rs`,
`policy-types/src/events.rs`):

- `PolicyGraphProjection::apply` is **infallible**: it calls the free function
  `apply_event_to_graph`, which returns `PolicyGraph`, not `Result`. The observer-failure → DLQ
  path is effectively unreachable for this subscriber (its realistic failure is a panic, e.g.
  graph lock poisoned, which unwinds the subscriber task — a separate hazard, out of scope here).
- So the live exposure is concentrated on the **deserialize-skip path**: any `policy_events` row
  whose JSON no longer fits the current `PolicyEvent` enum (variant renamed/removed without an
  upcaster) vanishes from the graph with a WARN and no gate. CLOUD-173's upcasting machinery is
  the intended defense; the fail-open default means a missed upcaster is silent.

## 2. Verified mechanism: the fail-open site inventory

All in `epoch_pg/src/event_bus/` on `main` @ `a19d001`.

### Deserialize-skip (3 independent sites)

1. **Live batch** — `process_subscriber_for_batch` (`mod.rs:268-296`): on
   `serde_json::from_value::<D>` failure, WARNs *"Advancing checkpoint past this event"*, inserts
   the sequence into `state.processed_ahead`, and `record_processed()`s the `Batched` counter.
   The skip is a **first-class fold-in**: `advance_contiguous_checkpoint`
   (`subscriber_state.rs:193`) advances across `processed_ahead` entries with the comment
   *"processed out-of-order (or skipped due to deser failure)"*. A skipped event is
   indistinguishable from an applied one.
2. **Catch-up pass** — `catch_up_from_checkpoint` (`mod.rs:2983-2997`): WARNs *"Advancing
   checkpoint past this event"*, then calls `advance_catchup_prefix` with the skipped sequence,
   which advances the contiguous prefix and flushes per `CheckpointMode`.
3. **`subscribe()` buffer drain** — (`mod.rs:3428-3441`): deser failure maps the row to `None`
   (event never applied), and the shared tail then runs `advance_catchup_prefix` for it — the
   code comment states outright it *"runs for both the deserialization-error path and the
   successful-processing path"* (`mod.rs:3479-3481`).

### Observer failure: DLQ-and-continue (all paths)

1. `process_event_with_retry` (`retry.rs`) retries `max_retries` times with backoff, inserts a
   DLQ row (`ON CONFLICT (subscriber_id, event_id) DO UPDATE`), fires the optional
   `on_dlq_insertion` callback, and returns `ProcessResult::{Success, SentToDlq}`.
2. The live caller **discards the result** (`mod.rs:323`: bare `.await;`). The catch-up and
   buffer-drain callers only `debug!`-log it (`mod.rs:3050-3057`, `mod.rs:3460-3467`). In every
   case `advance_catchup_prefix` / the per-event bookkeeping runs unconditionally afterwards, so
   the checkpoint advances regardless.
3. `retry.rs:93-104` already documents the intent: *"Consider making retry behavior configurable
   per-subscriber rather than globally … adding an optional `RetryPolicy` to the `EventObserver`
   trait with a default implementation."*

### Gap skips (by design, two classes)

1. `advance_contiguous_checkpoint` returns `SkippedGap`s. `FenceCleared` = snapshot-fence-proven
   permanent missing sequence (writer aborted / burned value): provably lossless, debug-logged
   only. `TimeoutBackstop` = `gap_timeout` fired while the fence was still pinned or unavailable:
   **potential data loss**, WARN + `epoch_event_bus_gap_timeouts` row + `on_gap_timeout`
   callback. Both classes advance the checkpoint.

### What is *not* broken (post-0026/0027)

The flush machinery is tight: every flush site (`flush_checkpoint`,
`flush_expired_checkpoints`, `flush_all_pending_checkpoints`) gates on
`PendingCheckpoint::is_publishable()`, and both the live and catch-up paths publish only the
contiguous prefix. The fail-open is therefore purely a question of **what counts as processed** —
the checkpoint pipeline below it can be reused as-is once the prefix is held correctly.

## 3. What fail-closed has to mean, per failure class

Discovery's proposed semantics (design direction, not settled):

| Failure class | Fail-open (today) | Fail-closed proposal |
|---|---|---|
| Deserialize failure | WARN, skip, checkpoint past | **Halt the subscriber**: hold the contiguous prefix at the bad sequence, apply nothing further for it |
| Observer failure (retries exhausted) | DLQ row + callback, checkpoint past | DLQ row + callback (observability), **hold** the prefix, halt |
| `TimeoutBackstop` gap | WARN + gap row + callback, advance | **Refuse the backstop**; hold until the fence clears (`FenceCleared`) or the gap fills |
| `FenceCleared` gap | debug log, advance | Accept (provably lossless; an event that never existed cannot be missed) |
| Inline `on_event` error | `publish()` already returns `Err` (aborting the cascade) | Unchanged — Inline is already fail-closed for observer errors (`mod.rs:2694`), and Inline never deserializes (events are typed in-process) |

Rationale for *halt, don't limp*:

- The contract is *"every event, in order"*. Continuing to apply **later** events while holding
  an unappliable earlier one violates it in the dangerous direction: for an auth oracle, applying
  a later grant while a revocation is stuck is precisely the over-grant being defended against.
- **Self-healing recovery comes free**: the live batch loop re-queries from
  `min(contiguous_checkpoint)` every batch (`mod.rs:1509-1524`), and a halted subscriber's
  checkpoint stays below the bad event, so the bus re-fetches and re-attempts it every batch.
  Once a deploy restores the variant/upcaster (or the observer is fixed), the next batch
  deserializes/applies and delivery resumes with **no manual replay tooling**. The DLQ row (and
  the halt itself) is the alert; recovery is redeploy-and-wait.
- Readiness is correct by construction: `subscriber_position` reads the contiguous checkpoint
  (spec 0026 R1), so a held wedge makes `wait_until_caught_up` / `wait_until_all_caught_up`
  block, and a CLOUD-221 readiness gate correctly refuses to certify. Needs an explicit test.

## 4. Where the opt-in lives

- Per-bus `ReliableDeliveryConfig` is the wrong altitude: a bus serves many subscribers, and the
  fail-open default is correct for most of them (metrics, notifications, best-effort read
  models).
- The trait already has the precedent hook: `EventObserver::subscription_mode()` →
  `SubscriptionMode` (epoch_core `event_store.rs:196-217`). Natural shape:
  `fn failure_mode(&self) -> FailureMode { FailureMode::FailOpen }` with
  `#[non_exhaustive] enum FailureMode { FailOpen, FailClosed }`. Resolved once in `subscribe()`
  alongside `subscription_mode()` (same single-lock discipline).
- Interaction with `CheckpointMode`: orthogonal. A held position never publishes (is_publishable
  - contiguous-only), so `Batched` + fail-closed is safe; redelivery-after-crash windows are
  unaffected (fail-closed defends against *skipping*, not duplication). Keep them independent.

## 5. Design constraints discovered (the hard parts)

1. **`min_checkpoint` fetch growth** (VERIFIED, `mod.rs:1509-1524`): the shared live fetch is
   bounded by the *minimum* contiguous checkpoint across all subscribers. A permanently wedged
   subscriber pins that floor, so the per-batch re-read window (wedge → head) grows forever, and
   every healthy subscriber re-skips those rows each batch. INFERRED mitigation options:
   (a) accept it — a wedged auth oracle is a degraded incident anyway and ops intervene;
   (b) exempt wedged subscribers from the group's `min_checkpoint` and give them a private narrow
   fetch (or a capped backoff re-attempt). The spec must pick one; (b) is the principled answer
   but adds a "wedged" state to the listener's bookkeeping.
2. **Mid-batch halt point** (INFERRED, from batch structure): on hitting the bad event the
   subscriber must stop processing its *remaining* batch events (do not apply later events), but
   the batch task itself should return promptly — it does NOT stall its priority group on time
   (the per-subscriber task completes fast; CLOUD-228's head-of-line blocking is about slow
   handlers, not halts). The group's cost is only the fetch floor in (1).
3. **Later priority groups keep running**: a halted priority-0 projection does not stop
   priority-100 sagas from consuming events the projection refused. Sagas will query a frozen
   read model (deny-heavy for an auth oracle — safe direction, but must be a documented
   consequence, not a surprise). Blocking cross-group is a much bigger design change; recommend
   out of scope, documented.
4. **`TimeoutBackstop` refusal interacts with `gap_timeout` semantics**: a fail-closed subscriber
   effectively needs fence-only gap resolution. A transaction held open longer than
   `gap_timeout` (default 5s) wedges it until commit — self-healing, but worth documenting, and
   it means fail-closed subscribers want a *longer* `gap_timeout` on their bus or none at all.
   No per-subscriber `gap_timeout` exists today (per-bus config only).
5. **Halt observability**: a halt that only logs is a halt nobody sees. Proposal: the deser-halt
   path also writes a DLQ row (new `error_message` kind, e.g. `unrecoverable: deserialize`) and/or
   a new `on_halt`-style callback; the observer-failure halt already has the DLQ row + callback.
   The spec must settle one mechanism so ops have exactly one place to look.
6. **ReplayAlways intersection (CLOUD-227)**: the ReplayAlways HWM is a linear max, not a
   contiguous prefix, so a fail-closed ReplayAlways subscriber could apply above an unappliable
   event via a listener restart (HWM survives in-process). Either scope 0028 to `Checkpointed`
   subscribers and defer the ReplayAlways variant to CLOUD-227's fix, or make the HWM contiguous
   in the same pass (same mechanism, different sink — cheap once 0028's hold logic exists, and
   would close CLOUD-227 as a side effect). Discovery leans toward including it.

## 6. Open questions for the spec

1. `FailureMode` flag vs a richer per-subscriber policy object (retry.rs's "Future Improvements"
   envisioned a `RetryPolicy`). One enum now, extensible later via `#[non_exhaustive]`?
2. Halt scope: subscriber-only (recommended) vs error out of `start_listener` (nuclear for a
   shared bus) vs priority-group halt.
3. Deser-halt observability: DLQ row, new callback, or both?
4. Should fail-closed refuse `TimeoutBackstop` unconditionally, or gate on a per-subscriber
   `gap_timeout` override?
5. Wedged-subscriber fetch strategy: accept growth vs private fetch path (constraint 5.1).
6. ReplayAlways/HWM: in-scope (closes CLOUD-227) or deferred to it?
7. Does the spec need to address the panic-in-`on_event` unwind path (a panicking observer kills
   the per-subscriber task / inline drain), or is that explicitly out of scope?

## 7. Test plan sketch

Discipline per 0026/0027: relative sequence values via `INSERT ... RETURNING global_sequence`,
no absolute assertions (`epoch_events` is shared across parallel test binaries).

- **Live deser halt**: bus with a fail-closed subscriber; commit an event whose data cannot
  deserialize into `D`, then later, deserializable events. Assert: checkpoint held below the bad
  sequence, later events NOT applied, other subscribers on the same bus unaffected and advancing,
  `wait_until_caught_up` for the halted subscriber returns/times out as not-caught-up, DLQ/alert
  mechanism fired.
- **Live auto-recovery**: same setup, then make the event deserializable again (e.g. write a
  corrected payload at the held sequence, simulating the deploy that restores the variant).
  Assert: the next batch applies it and everything after it, exactly once, in order.
- **Observer-failure halt**: observer whose `on_event` always fails; assert DLQ row + callback +
  held checkpoint + no advance after retries exhaust; then fix the observer and assert recovery.
- **Catch-up halt**: same two scenarios against the `subscribe()` catch-up pass and the buffer
  drain.
- **Backstop refusal**: fail-closed subscriber + gap held past `gap_timeout` with the fence
  pinned; assert no advance while unproven; assert advance once `FenceCleared`.
- **Batched interplay**: fail-closed + `Batched`; assert no flush above a held wedge while
  `max_delay_ms` expires repeatedly (is_publishable should make this pass by construction).
- **ReplayAlways** (if in scope): fail-closed ReplayAlways subscriber must hold its HWM below a
  deser failure across a listener restart.
