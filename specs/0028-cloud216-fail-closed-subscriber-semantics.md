# Spec 0028: Per-Subscription Fail-Closed Delivery Semantics

**Issue:** CLOUD-216 · **Status:** Draft · **Crate:** `epoch_core` (trait surface), `epoch_pg` (bus mechanics) · **Scope:** `feat(core)`, `feat(pg)` — additive public API, no schema migration · **Input:** brief 0028 + five probe reports + review round 1 findings (evidence: `~/.pi/agent/sessions/--root-code-epoch-worktrees-cloud-216--/subagent-artifacts/outputs/90fb4050-*/probe-{A..E}-*.md`) · **Anchored to:** `a19d001` · **Sequel to:** specs 0026/0027.

All line anchors below were probe- and review-verified against source at `a19d001`; drift since
then is possible and the implementation plan must re-anchor.

## 1. Problem

`epoch_pg`'s Postgres event bus is unconditionally **fail-open**: on every failure path it logs,
skips, and advances the checkpoint past the event anyway. No subscriber can opt into the opposite
contract: *"if I cannot correctly apply every event in order, do not silently move on."*

The motivating consumer is catacloud's `PolicyGraphProjection` (the authorization graph read on
every `policy.evaluate()`/`scope()` call). Since CLOUD-217 it runs on the standard bus path, and
its old abort-on-decode-failure invariant was deleted, not relocated. Its `apply` is infallible,
so the live exposure is the **deserialize-skip path**: a `policy_events` row whose JSON no longer
fits the current `PolicyEvent` enum (variant renamed/removed without an upcaster) vanishes from
the graph with a WARN and no gate. A silently skipped revocation is a silent over-grant.
CLOUD-173's upcasting is the intended defense; the fail-open default is what makes a missed
upcaster silent.

### 1.1 Mechanism: the fail-open site inventory (probe A: all CONFIRMED)

In `epoch_pg/src/event_bus/`:

- **Deserialize-skip, 3 independent sites** — live batch (`process_subscriber_for_batch`,
  mod.rs:280-293: fold into `processed_ahead` + `record_processed`), catch-up
  (`catch_up_from_checkpoint`, mod.rs:2977-3007, `advance_catchup_prefix` past it at 2994), and
  the `subscribe()` buffer drain (mod.rs:3425-3438 deser; shared tail
  `advance_catchup_prefix` at mod.rs:3484). `advance_contiguous_checkpoint` folds
  `processed_ahead` entries identically (subscriber_state.rs:192-198), so a skipped event is
  indistinguishable from an applied one.
- **Observer failure: DLQ-and-continue on every path** — `process_event_with_retry` (retry.rs)
  retries, DLQs (`ON CONFLICT DO UPDATE`), fires `on_dlq_insertion`; the live caller discards the
  result (mod.rs:318-331), catch-up (mod.rs:3030-3045) and drain (mod.rs:3484) only debug-log,
  and advancement runs unconditionally.
- **Gap skips** — `FenceCleared` (subscriber_state.rs:222-241, provably lossless, debug-only) and
  `TimeoutBackstop` (subscriber_state.rs:246-262, potential loss, WARN +
  `epoch_event_bus_gap_timeouts` row + `on_gap_timeout`) both advance the checkpoint (handling
  mod.rs:344-410).

### 1.2 What is already tight (post-0026/0027, probe A/C: CONFIRMED)

Every flush site publishes only the publishable contiguous prefix: `try_flush_pending_checkpoint`
(mod.rs:2745), `flush_expired_checkpoints` (checkpoint.rs:205), `flush_all_pending_checkpoints`
(checkpoint.rs:239); the catch-up/drain final flushes are safe by constructor invariants. The
fail-open is purely a question of **what counts as processed** — the checkpoint pipeline below is
reusable as-is. Readiness is wired to the contiguous checkpoint (`subscriber_position` →
`get_checkpoint`, mod.rs:1737-1887), so a held wedge blocks `wait_until_caught_up` by
construction.

### 1.3 Severity

Fail-open is correct for most subscribers. The gap is that an authorization-critical subscriber
has no escape hatch, and the current consumer (catacloud policy graph) needs one today.

## 2. What must NOT break

- **The 0026/0027 flush machinery is untouched**: every flush site keeps its
  `is_publishable`/contiguous-prefix gating; fail-closed only changes what feeds it.
- **Fail-open remains the default and is behaviour-preserved** on every path (deser-skip,
  DLQ-and-continue, both gap classes) — regression-pinned. One deliberate exception, documented
  in §3.6: panic containment keeps the listener alive where today it dies.
- **Inline mode is unchanged** — already fail-closed for observer errors (`publish()` returns
  `Err`, aborting the cascade) and never deserializes (typed in-process events).
- **`FenceCleared` still advances under both modes**: an event proven never to have existed
  cannot be missed.
- **The two advancers stay separate** (0027 §2): `advance_catchup_prefix` remains a fence-less
  exact-match advance; `advance_contiguous_checkpoint` keeps fence/backstop ownership.
- **`update_checkpoint` (public deliberate-rewind API) is unchanged in signature, behaviour, and
  reachability.** The new operator release path (§3.4/§3.5, R14) is a separate, dedicated
  release operation — it does not repurpose or reinterpret `update_checkpoint`.
- **Redelivery-after-crash windows are unaffected**: fail-closed defends against *skipping*, not
  duplication; `CheckpointMode` and `FailureMode` stay orthogonal (`Batched` + fail-closed is
  safe via `is_publishable`, pinned by test T6).

## 3. Design

### 3.1 The contract: halt, don't limp

On a failure, a fail-closed subscriber **holds** its contiguous prefix at the bad sequence,
applies nothing further (for it), and keeps the position below the failure so the bus re-attempts
it every batch. Applying *later* events while holding an earlier unappliable one is rejected: for
an auth oracle, applying a later grant while a revocation is stuck is precisely the over-grant
being defended against.

### 3.2 The opt-in, and reaching the motivating consumer (probe D placement + forwarding-chain fix)

Placing `failure_mode()` on `EventObserver` alone is unreachable for the motivating consumer:
`PolicyGraphProjection` is a `Projection`, wrapped by `ProjectionHandler` (the actual
`EventObserver` registered with the bus), so a bare `EventObserver::failure_mode()` would resolve
to the trait default (`FailOpen`) forever, regardless of what the projection wants.
`SubscriptionMode` hit the identical reachability problem and solved it with a forwarding chain;
`FailureMode` follows the same shape:

```rust
// epoch_core/src/event_store.rs, alongside SubscriptionMode (enum at :196, method at :258)
#[non_exhaustive]
pub enum FailureMode { FailOpen, FailClosed }

// Default body on EventObserver, Projection, and Saga alike:
fn failure_mode(&self) -> FailureMode { FailureMode::FailOpen }
```

Mirroring the `subscription_mode()` chain — `Projection::subscription_mode` (`projection.rs:180`),
`ProjectionHandler` forwarding (`projection.rs:260-261`), `Saga::subscription_mode` (`saga.rs:135`),
the `Arc` blanket impl (`saga.rs:230-231`), `SagaHandler` (`saga.rs:297-298`), and the saga
adapter (`saga.rs:458-459`):

- `Projection` and `Saga` each get their own `failure_mode()` default method.
- `ProjectionHandler`/`SagaHandler`'s `EventObserver::failure_mode` implementations
  (`projection.rs:247`, `saga.rs:279`) forward to the wrapped projection's/saga's
  `failure_mode()`, instead of falling through to the `EventObserver` default; the
  `SagaAdapter`'s implementation (`saga.rs:417`) forwards the same way.
- A downstream author overrides `failure_mode()` on their `Projection`/`Saga` impl; the handler
  wrapper carries it through automatically — no `EventObserver`-level override needed for the
  common case.

`subscribe()` still resolves the value once, from the observer (whose `failure_mode()` now
reflects the forwarded choice), inside the existing single-lock block (mod.rs:3152-3155,
`let (subscriber_id, mode) = { let o = observer.lock().await; ... }` — extend to
`(subscriber_id, mode, failure_mode)`). Per-bus config is the wrong altitude: one bus serves many
subscribers, and fail-open stays correct for most. Every default body means every existing
implementor — the public ones (`SagaHandler`, `ProjectionHandler`) and every downstream
`EventObserver`/`Projection`/`Saga` implementor — compiles unchanged.

### 3.3 Halt mechanics per path (probe A: intervention points CONFIRMED)

| Path | Today (fail-open) | Fail-closed intervention |
|---|---|---|
| Live deser (mod.rs:280-293) | fold into `processed_ahead`, `record_processed`, continue | Do not fold/count; write deser DLQ row + fire `on_halt`; stop consuming this subscriber's remaining batch rows |
| Catch-up deser (mod.rs:2977-3007, advance at 2994) | `advance_catchup_prefix` past it | Skip the advance; halt the catch-up loop at the bad sequence |
| Drain deser (mod.rs:3425-3438, shared tail 3484) | map to `None`, shared tail advances | Skip the shared `advance_catchup_prefix` tail for the failed row; halt buffer processing — but still complete subscriber registration: the observer is pushed onto the live `projections` list at mod.rs:3560, *after* the drain loop and final flush, so an early return out of `subscribe()` on a drain halt must not skip that registration, or the subscriber is never driven by the listener and can never self-heal (defeats §3.4/R6) |
| Observer exhausted (retry.rs; callers mod.rs:318-331, 3030-3045, 3484) | DLQ + continue | DLQ + `on_dlq_insertion` still fire, plus `on_halt`; hold (no `processed_ahead`/`record_processed`, no `advance_catchup_prefix`); a re-attempt of an already-held event costs at most one observer invocation per batch cycle — it does not re-run the full retry ladder (§3.4) |
| `TimeoutBackstop` (subscriber_state.rs:246-262) | advance past gap | **Refuse inside `advance_contiguous_checkpoint`**: the advance happens there, so refusal must be a policy parameter to it (probe C / review), not a call-site decision — `break` instead of advance, keep `gap_first_seen` |
| `FenceCleared` (subscriber_state.rs:222-241) | advance | Unchanged (accept) |

Recovery is **self-healing** (§3.4), and a held wedge does not starve other subscribers (§3.5).

### 3.4 Recovery is self-healing

Once the blocking cause is fixed (upcaster deployed → payload deserializes; observer fixed; row
corrected), the next batch re-fetches from the held checkpoint, the event applies, and delivery
resumes exactly-once, in order. No manual replay tooling required for this case. The halt signal
(§4 Q3) is the alert; recovery is redeploy-and-wait.

A re-attempt of an already-held observer-failure event does **not** re-run the full retry ladder:
at most one `on_event` invocation per batch cycle for a held event. The ladder's backoff
(`max_retries: 3`, `initial_retry_delay: 1s`, `max_retry_delay: 60s`, config.rs:256-258) earns its
keep on the *first* failure, where it distinguishes a transient error from a real one; re-running
all `max_retries` attempts every batch would cost ~7s of sleep per batch and throttle the whole
listener loop (`join_all` at mod.rs:1633, the sequential priority-group loop at mod.rs:1596) to
one batch per ~7s for as long as the halt lasts. This is a throttling hazard the design avoids by
construction, not a cost the spec accepts.

Fail-closed means no *silent* skip, not no skip ever. For a genuinely lost sequence — an
abandoned/prepared transaction pinning `xmin` past `fence_xmax`, the exact case the
`TimeoutBackstop` exists for (mod.rs:388-400) — the documented operator remedy is a dedicated,
explicit **release operation** on the bus (public API, name TBD at plan time) that advances a
wedged subscriber's persisted position *past* the held sequence: an explicit, audited operator
acceptance of the skip, not a silent one — the audit artifact is a WARN log plus the halt
callback fired with a `HaltReason::Released` variant on the same `HaltInfo` surface (§4 Q3).
`update_checkpoint` (mod.rs:1758-1764) is deliberately
left out of this role — its own rustdoc instructs callers to pass only a sequence the subscriber
has actually finished processing up to a contiguous prefix, and a release is, by definition, the
opposite: an operator choosing to skip past a sequence the subscriber never finished. Per R14
(§3.5), the release operation takes effect on a running bus without a process restart.

### 3.5 Halt is subscriber-local, and does not starve healthy peers

A halt stops only that subscriber's own delivery. Priority groups stay sequential across groups
and concurrent within (outer loop mod.rs:1596, `join_all` mod.rs:1633): a halted priority-0
subscriber does not stall its group on time (the task returns promptly) and does not stop
priority-100 groups. Documented consequence: later groups keep consuming events the halted group
refused — sagas query a frozen read model (deny-heavy for an auth oracle: fails safe).

The live batch's shared fetch, however, is **not** subscriber-local by default: it queries
`WHERE global_sequence > min_checkpoint ... LIMIT catch_up_batch_size` (mod.rs:1508-1527, default
limit 100 at config.rs:261), where `min_checkpoint` is the minimum contiguous checkpoint across
**all** subscribers (mod.rs:1508-1513). A permanently wedged fail-closed subscriber pins that
floor, so the shared window never advances past `[wedge, wedge + catch_up_batch_size]`: healthy
peers re-fetch the identical rows, skip them (`event_seq <= contiguous_before`, mod.rs:257-259),
`any_subscriber_processed` stays false, and the loop breaks (mod.rs:1660). That is a **liveness
failure** — every healthy subscriber on the bus is permanently starved, not merely slowed — and
it inverts the whole fail-safe argument: a wedged auth projection would freeze every other read
model on the bus. The group-stall shape (one slow handler stalling its own priority group) is
pre-warned in the bus's own rustdoc (mod.rs:2036-2044); the bus-wide floor starvation identified
in review is new to this design, not something the existing rustdoc already covers.

Design (this is a scope change from the original draft's Q5 — see §4; scoped to **`Checkpointed`
subscribers**, see §4 Q6/R9 for the `ReplayAlways` analogue): a wedged (halted) subscriber is
excluded from the shared `min_checkpoint` floor and instead fetches privately, from its own
persisted checkpoint, with a small batch cap. The private fetch re-seeds *only its fetch cursor*
from the persisted checkpoint each cycle; it does **not** discard the subscriber's wedge
bookkeeping. Specifically preserved across re-seeds:

- the held contiguous position itself;
- `processed_ahead` — events already applied above a refused (`TimeoutBackstop`) gap
  (mod.rs:325 insert, folded at subscriber_state.rs:191-197). These were genuinely applied;
  discarding this set and re-fetching from the persisted checkpoint would re-deliver them to
  `on_event`, breaking R6's exactly-once guarantee;
- `gap_first_seen`, **including the `fence_xmax` snapshot captured at first observation**
  (first-observation insert at subscriber_state.rs:266-273; the lazy backfill at 211-215 fires
  only when `fence_xmax.is_none()`, so a re-seed must additionally not reset a still-`None`
  fence — that would re-arm the backfill) — never re-captured on a later re-seed. `FenceCleared`
  fires only
  when `snap.xmin >= fence_xmax` against that first-captured value; re-capturing it against a
  moving `xmax` on every cycle would re-arm the fence indefinitely on a bus with continuous write
  traffic, and the promised automatic recovery for gap wedges (Q4) would never fire;
- the held-event marker that implements §3.4's one-invocation-per-batch-cycle rule for an
  observer-failure wedge — discarding it on every re-seed would re-run the full retry ladder
  every cycle, defeating that guarantee.

The one case the private fetch *does* reconcile against the persisted checkpoint: if the
persisted cursor has moved forward (an operator release, see §3.4), fold `processed_ahead`
entries at or below the new cursor into the contiguous position, adopt the new position, and
retain any remaining `processed_ahead` entries above it — so events already applied while wedged
are still delivered exactly once, never twice.

The private fetch also builds its **own** batch context — its own `visible_seqs` and txid
snapshot, from its own query's rows — rather than reusing the shared window's `shared_visible`
(mod.rs:1543/1560, handed to every subscriber at mod.rs:1621 and consumed by gap detection at
mod.rs:342 / subscriber_state.rs:201-206). Once the shared window floats above the wedge, every
sequence between the wedge and that window would otherwise look like a gap to the wedged
subscriber — spurious `gap_first_seen` entries, and spurious `TimeoutBackstop` skips for any
fail-open subscriber temporarily excluded from the shared floor for an unrelated reason.

If **every** subscriber on the bus is wedged, the shared fetch is skipped entirely for that
cycle (there is no healthy consumer left to serve); every subscriber instead proceeds through its
own private fetch (a wedged ReplayAlways subscriber proceeds from its in-memory HWM). This pins
the `min().unwrap_or(0)` empty-set case (mod.rs:1513).

This private, persisted-checkpoint-driven fetch is also what makes the operator release
operation (§3.4, R14) effective **in-process** on a running bus: today `subscriber_states`
(seeded once at mod.rs:1428-1460) stay in memory and are never re-read, so without this private
re-seed there would be no way to unwedge a live subscriber short of a process restart. A wedged
`ReplayAlways` subscriber has no persisted checkpoint row to re-seed from; its in-process HWM is
the position of record, so its remedy is a fresh `subscribe()` call (HWM resets to 0 per
mod.rs:3251, full replay re-holds at the same blocker per R9b) rather than a new release API —
a plan-level detail, not additional public surface.

### 3.6 Panic containment (probe B: CRITICAL pre-existing hazard)

Today there is **no `catch_unwind` anywhere** (verified by grep). A panic in `on_event` unwinds
the per-subscriber future through `join_all` into the listener loop, killing the entire listener
task; on the inline path it unwinds through `publish()` to the caller. For the motivating
consumer this is not hypothetical: a poisoned graph lock is `PolicyGraphProjection`'s realistic
failure mode, and fail-closed's promise ("halt the subscriber, keep the bus running") is void if
a panic kills the bus.

Design: wrap each `on_event` invocation in `AssertUnwindSafe(..).catch_unwind()` inside
`process_event_with_retry` (retry.rs) and in the inline drain, classifying a panic as an observer
failure. Under `FailClosed` the exhaustion path halts (§3.3). Under `FailOpen` the existing
retry → DLQ-and-continue semantics apply — a deliberate, documented behaviour change: the
listener survives where today it dies. `tokio::sync::Mutex` does not poison, so the observer lock
is released on unwind; a panicking observer's subsequent re-invocations re-panic and are bounded
by `max_retries` per event per batch.

The inline path has no listener to survive; there, a caught panic instead routes through
`publish()`'s existing `Err` branch (`inline_state` cleanup at mod.rs:2695-2699 — `in_progress`
reset, `entry.done` notified). That cleanup is load-bearing: without it, a swallowed panic would
leave `in_progress` stuck `true` and `entry.done` never notified, deadlocking the bus.

## 4. Decided open questions

- **Q1 — flag vs policy object: `FailureMode` enum.** One `#[non_exhaustive]` enum now with a
  default method; the `RetryPolicy` idea from retry.rs's future note stays future. Extensible
  without breaking downstream implementors.
- **Q2 — halt scope: subscriber-only.** Erroring out of `start_listener` is nuclear for a shared
  bus; group halt couples unrelated subscribers. Cross-group consequence documented (§3.5).
- **Q3 — deser-halt observability: DLQ row + `on_halt` callback (probe D recommendation).** New
  `HaltCallback` trait + `HaltInfo { subscriber_id, held_below_sequence, reason: HaltReason }`
  in `epoch_pg/src/event_bus/config.rs`, parallel to `DlqCallback`/`GapTimeoutCallback`. Deser
  halt also writes a DLQ row with `error_message = "unrecoverable: deserialize: <err>"` (TEXT
  prefix convention — no migration; `resolved_at`/`resolved_by` columns from m006 give ops the
  audit trail). Observer-exhaustion halt reuses its existing DLQ row and adds `on_halt`. Gap
  refusal writes no DLQ row (no event exists) — WARN + `on_halt`. One place to look: the DLQ
  table + `on_halt`.
- **Q4 — backstop refusal: unconditional, no per-subscriber `gap_timeout` override; operator
  remedy is an explicit forward release, not indefinite silent limbo.** Refusal is a property of
  the failure mode, not a tuning knob; a fail-closed subscriber holds while the fence is pinned
  regardless of duration. `gap_timeout` stays per-bus (config.rs:198) and remains the operator's
  lever for buses hosting fail-closed subscribers with long transactions. Fence proven-clear
  (`FenceCleared`) or gap fill resumes delivery automatically — this remains the automatic path;
  for a gap that never resolves (the abandoned/prepared-transaction case the backstop exists
  for), the documented remedy is an explicit forward release via the dedicated release operation
  (R14; see §3.4/§3.5) — fail-closed means no *silent* skip, not no skip ever.
- **Q5 — wedged fetch: private fetch is IN SCOPE (flips the original "accept growth" call).**
  Review found the shared-floor design (originally accepted as "option a") is not a performance
  cost but a liveness bug: a permanently wedged subscriber pins the shared `min_checkpoint` floor
  and starves every healthy subscriber on the bus (§3.5). Brief option (b) — exclude wedged
  subscribers from the shared floor and give them a private, re-seeding fetch — is adopted
  (R13, R14, T9, phase P4b).
- **Q6 — ReplayAlways: in scope; contiguous HWM closes CLOUD-227.** Probe D corrected the brief:
  the HWM (`Arc<Mutex<HashMap>>`, mod.rs:811) does **not** survive `subscribe()` — it resets to 0
  (mod.rs:3251), so a fresh `subscribe()` already replays from scratch; a listener *restart*
  without a fresh `subscribe()` re-seeds the surviving in-process HWM instead (mod.rs:1449) —
  see R9(a)/(b). The linear-max hole is solely in `advance_catchup_prefix` (mod.rs:2828-2833),
  which sets `hwm = event_global_seq` *before* the `!= *contiguous + 1` guard; the live path is
  already contiguous (mod.rs:494-499, spec 0027). Closing that one function is cheap
  (~50-100 LOC) and closes CLOUD-227's substance (HWM is a linear max, not a contiguous prefix)
  as a side effect; confirm its exact wording at review.
- **Q7 — panic path: in scope, as §3.6.** Minimal containment, no retry-policy generalization.

## 5. Requirements

- **R1** `FailureMode` enum defined once in `epoch_core`, with default `FailOpen` bodies on
  `EventObserver`, `Projection`, and `Saga`; `ProjectionHandler`/`SagaHandler` (and the `SagaAdapter`)
  forward the wrapped projection's/saga's `failure_mode()` (mirroring the `subscription_mode()`
  chain, including the `Arc<S>` blanket impl at saga.rs:226-232 — an `Arc`-wrapped saga must not
  silently lose the override); resolved per-subscription at BOTH delivery sites — the
  `subscribe()` single-lock block (which the catch-up/drain halts of R3 run inside) and the
  listener's state-init capture (mod.rs:1428-1434, which the live loop reads);
  existing implementors compile unchanged.
- **R2** Fail-closed deser failure on the live path holds the contiguous prefix at the bad
  sequence; no later event is applied for that subscriber in that batch.
- **R3** Same hold for the catch-up pass and the `subscribe()` buffer drain; a drain halt still
  completes subscriber registration (the observer is still pushed onto the live `projections`
  list) so the subscriber is driven by the listener and can self-heal.
- **R4** Fail-closed observer-failure (retries exhausted) holds after DLQ row +
  `on_dlq_insertion`; a re-attempt of an already-held event costs at most one observer invocation
  per batch cycle, not the full retry ladder.
- **R5** Fail-closed refuses `TimeoutBackstop` (no advance, no gap-timeout row/callback for it)
  and accepts `FenceCleared`.
- **R6** Recovery is self-healing: after the cause is fixed, the held event and everything after
  it are applied exactly once, in order, without manual replay; for a gap that never resolves,
  an explicit operator release (R14) is the documented manual remedy.
- **R7** Every halt is observable: deser halt → DLQ row (`unrecoverable: deserialize`) +
  `on_halt`; observer halt → existing DLQ row + `on_halt`; gap refusal → WARN + `on_halt`.
- **R8** A held wedge blocks readiness: `subscriber_position` stays below head and
  `wait_until_caught_up`/`wait_until_all_caught_up` do not certify (CLOUD-221 interplay).
- **R9** Fail-closed ReplayAlways holds a **contiguous** HWM below a failure: (a) the hold
  survives a listener restart, since the in-process HWM is re-seeded rather than reset
  (mod.rs:1449); (b) a fresh `subscribe()` call resets the HWM to 0 (mod.rs:3251) and re-holds at
  the same event on the resulting full replay.
- **R10** Fail-open default preserves today's delivery/checkpoint behaviour on every path
  (regression-pinned), with the single documented exception of panic containment (R11).
- **R11** A panic in `on_event` is caught and classified as an observer failure on the live,
  catch-up, and drain paths (the listener survives); on the inline path the caught panic instead
  routes through the existing `Err` branch of `publish()` (`inline_state` cleanup at
  mod.rs:2695-2699 — `in_progress` reset, `entry.done` notified — otherwise the bus deadlocks).
- **R12** The frozen-read-model cross-group consequence of a halt is documented in rustdoc on
  `FailureMode`.
- **R13** Healthy subscribers keep advancing while another subscriber is wedged: the shared
  `min_checkpoint` floor excludes wedged subscribers, so a permanent hold on one subscriber does
  not bound the fetch window (and therefore delivery) for any other subscriber on the bus. This
  applies to `ReplayAlways` subscribers too, not only `Checkpointed` ones: `subscriber_states` is
  seeded for `ReplayAlways` subscribers as well (`contiguous_checkpoint` set from the HWM value,
  mod.rs:1436-1450) and the floor computation (mod.rs:1509-1513) has no mode filter, so a wedged
  `ReplayAlways` subscriber pins the bus-wide floor exactly like a `Checkpointed` one once it has
  a contiguous hold (§4 Q6/P5). The floor exclusion therefore must extend to a held `ReplayAlways`
  HWM; only the release *operation* (not the exclusion) stays `Checkpointed`-only, since a
  `ReplayAlways` subscriber has no persisted checkpoint row to release — its remedy is a fresh
  `subscribe()` call (R9b).
- **R14** Operator release: a dedicated release operation advancing a wedged (`Checkpointed`)
  subscriber's persisted position *past* the held sequence takes effect on a running bus without
  a process restart (the private re-seeding fetch of §3.5 picks up the new cursor on its next
  cycle); events already applied above the released position (`processed_ahead`) are still
  delivered exactly once, never re-delivered (R6).

## 6. Behavioural risk owned

- **Wedged-subscriber starvation is designed against, not accepted.** Excluding a halted
  subscriber from the shared `min_checkpoint` floor and giving it a private re-seeding fetch
  (§3.5) prevents the liveness failure a shared floor would otherwise cause (R13). Residual cost
  is the small private-fetch query itself while wedged — bounded, not growing.
- **Panic containment** changes fail-open behaviour (listener survives instead of dying; DLQ
  row instead of task death). Flagged for review sign-off.
- **`Batched` + fail-closed** must not flush above a held wedge while `max_delay_ms` fires
  repeatedly — expected to pass by construction (`is_publishable`), pinned by T6.
- **DLQ row volume** for deser halts: one row per bad event per subscriber, deduped by
  `ON CONFLICT (subscriber_id, event_id)` — bounded by distinct bad events.
- **Retry-ladder re-runs are bounded to one invocation per batch cycle** for an already-held
  event (§3.4) — avoided as a throttling hazard by construction, not accepted as a cost.

## 7. Test plan (paper)

Discipline per 0026/0027: relative sequence values via `INSERT ... RETURNING global_sequence`
(`insert_committed_event`), `isolated_events_table` for gap tests, no absolute sequence
assertions. Probe E surveyed all facilities (file:line in its report); feasibility verdict:
**all sketches implementable; 4 thin test utilities needed, no schema/migration**.

New utilities (in `epoch_pg/tests/`): `FailingObserver` (always `Err`), `PanickingObserver`
(panic in `on_event`), `CapturingDlqCallback`/`CapturingHaltCallback` (`Arc<Mutex<Vec<_>>>`
capture), `fix_event_payload` (`UPDATE <table> SET data = $1 WHERE global_sequence = $2`).

| # | Test | Setup sketch | Key assertions |
|---|---|---|---|
| T0 | Forwarding-chain reachability (R1) | Custom `Projection` overrides `failure_mode() -> FailClosed`, wrapped in `ProjectionHandler` (epoch_core unit test, no bus) | `EventObserver::failure_mode()` on the handler returns `FailClosed` — the override reaches through the handler, not the `EventObserver` default; `subscribe()`-level resolution is behaviorally verified by T1's halt |
| T1 | Live deser halt | Fail-closed + normal subscriber; valid(N) → corrupt(N+1) → valid(N+2) | FC checkpoint == N; FC applied only N; normal subscriber advanced to N+2 (skipped N+1); `wait_until_caught_up(FC)` → `Ok(false)`; DLQ row + `on_halt` fired |
| T2 | Live auto-recovery | T1, then `fix_event_payload(N+1)` | Next batch applies N+1, N+2 exactly once, in order; checkpoint reaches N+2 |
| T3 | Observer-failure halt | `FailingObserver` (fail-closed) + healthy peer; retries exhaust | DLQ row (retry_count == max) + `on_dlq_insertion` + `on_halt`; checkpoint held; peer unaffected; **during the halt, the failing observer is invoked at most once per subsequent batch cycle (R4)**; then fix observer → recovery (R6) |
| T4 | Catch-up + drain halt | `setup_without_listener`, pre-planted checkpoint, corrupt row in history; start listener (catch-up), then live drain | Halt at bad sequence in both phases; no advance past it; DLQ + `on_halt`; readiness blocked; subscriber still registered post-drain-halt (R3) |
| T5 | Backstop refusal | Isolated table; `claim_hole_uncommitted`; `gap_timeout: 500ms`; fencing on | No advance after backstop would fire (fail-open would advance); no gap-timeout row; **`on_halt` fired on the refusal (R7)**; **roll back** the held txn (committing would make the sequence visible and deliver it as a normal event — no gap is ever detected, so no `FenceCleared` could fire; only an abort proves the sequence never existed) → `FenceCleared` → advance |
| T6 | Batched interplay | `CheckpointMode::Batched`, halt at corrupt row; wait several `max_delay_ms` periods | No flush above the wedge; after fix, flushes resume |
| T7 | ReplayAlways hold across listener restart (R9a) | Fail-closed `TestProjection::replay_always()`; corrupt row; `shutdown()` + restart listener (same subscribe, no fresh call) | In-process HWM re-seeds (not reset) across restart; held below bad row before and after; re-attempted (not skipped) on the next batch |
| T7b | ReplayAlways hold across fresh subscribe (R9b) | Same halt, then a fresh `subscribe()` call (HWM resets to 0 per mod.rs:3251) | Full replay from 0; halts again at the same bad row; no data applied above it despite replaying from scratch |
| T8 | Panic containment | `PanickingObserver` fail-closed; second variant fail-open; third variant on the inline path | Listener task alive for live/catch-up/drain; fail-closed: halt + DLQ + `on_halt`; fail-open: DLQ + continue (documented change); inline: `publish()` returns `Err`, `inline_state` cleaned up (no deadlock) |
| T9 | Wedge does not starve peers; operator release | Fail-closed **`Checkpointed`** subscriber wedged at seq X (gap wedge via `claim_hole_uncommitted`, or a deser wedge); healthy peer on same bus; commit events well beyond `catch_up_batch_size` past X | Peer advances past X + `catch_up_batch_size` while the wedge holds (R13); then invoke the operator **release** operation past X; assert the wedged subscriber resumes without a restart, with no duplicate delivery of any `processed_ahead` entries applied while wedged (R14 + R6) |
| R | Fail-open regression pins | Existing `test_undeserializable_event_advances_past` + new pins for observer-DLQ-and-continue and gap classes | Today's behaviour byte-for-byte (R10) |

## 8. Phasing (TDD; each phase: failing test → implement → refactor)

1. **P1 — trait surface** (`epoch_core` only): `FailureMode` enum with default `FailOpen` bodies on
   `EventObserver`, `Projection`, and `Saga`; `ProjectionHandler`/`SagaHandler`/`SagaAdapter`
   forwarding, including the `Arc<S>` blanket impl (saga.rs:226-232), mirroring the full
   `subscription_mode()` chain. Test T0 (epoch_core unit test) proving a custom `Projection`'s
   `failure_mode()` override reaches through `ProjectionHandler`. No epoch_pg changes in this
   phase; `subscribe()`-level resolution lands in P2 with its first consumer (clippy).
2. **P2 — resolution + live hold + observability** (`epoch_pg`): resolve `failure_mode` in the
   single-lock block and capture it on `SubscriberState` at state-init; live deser halt +
   observer-exhaustion hold (single-invocation-per-batch re-attempt, §3.4); `HaltCallback`/
   `HaltInfo` + deser DLQ row; panic containment in `process_event_with_retry`. Resolution is
   behaviorally verified by T1's halt. Tests T1, T2, T3, T6, T8.
3. **P3 — catch-up + drain hold**: gates at the catch-up (mod.rs:2977-3007) and drain
   (mod.rs:3425-3438/3484) sites, drain halt still completing registration (R3), plus panic wrap
   in the inline drain. Tests T4, remainder of T8.
4. **P4 — gap refusal**: policy parameter on `advance_contiguous_checkpoint`; refuse backstop,
   accept fence. Test T5 (T6 lands in P2, which builds the `Batched`-capable live hold it
   exercises).
5. **P4b — wedged-subscriber private fetch + operator release**: scoped to `Checkpointed`
   subscribers; exclude wedged subscribers from the shared `min_checkpoint` floor; private,
   checkpoint-re-seeding fetch (own `visible_seqs`/txid snapshot, preserved wedge bookkeeping,
   §3.5) with a small batch cap; new dedicated release operation (§3.4/R14, distinct from
   `update_checkpoint`) that unwedges a running listener without a restart. Test T9 (closes
   R13/R14). Plan note: in an all-wedged cycle the shared-row break conditions (mod.rs:1536,
   mod.rs:1660) do not apply — the cycle ends when all private fetches complete, and the timer
   tick keeps cycles firing.
6. **P5 — ReplayAlways contiguous HWM**: `advance_catchup_prefix` (mod.rs:2828-2833) sets
   `hwm = event_global_seq` before the `!= *contiguous + 1` guard — route it through the same
   contiguous-prefix hold machinery as checkpoints. Scoped entirely to that one function; the
   live path is already contiguous via `new_contiguous` (mod.rs:494-499, spec 0027) and needs no
   change. Tests T7, T7b (closes CLOUD-227).
7. **P6 — regression pins + docs**: R10 pins, `FailureMode` rustdoc (R12), `cargo fmt`/`clippy
   -D warnings`, full suite.

## 9. Acceptance criteria

- All paper tests T1-T9 (incl. T7b) + regression pins implemented and green; existing suite
  green.
- `cargo clippy -- -D warnings` and `cargo fmt` clean; no `unwrap()`/`expect()` outside tests.
- R1-R14 each traceable to a numbered test or an explicitly named verification (T0 for R1;
  a rustdoc check for R12); R8/R9 explicitly asserted (readiness block, HWM hold across restart
  and across a fresh `subscribe()`); R13/R14 explicitly asserted (peer liveness under a wedge,
  in-process operator release).
- No schema migration required; `FailureMode`, `HaltCallback` public API carries rustdoc.

## 10. Out of scope

- Blocking cross-priority-group on halt (documented consequence instead, §3.5).
- Per-subscriber `gap_timeout` override (Q4); per-subscriber `RetryPolicy` (Q1).
- Catacloud-side changes (its CLOUD-217 gate consumes this).
- DLQ retention/management changes.

## 11. Residual uncertainty

- The `advance_catchup_prefix` (mod.rs:2828-2833) HWM-hold implementation is the sole P5 target;
  the live path is already contiguous (mod.rs:494-499, spec 0027) and needs no change.
- `catch_unwind` ergonomics with `AssertUnwindSafe` over the observer future — plan-level detail;
  semantics specified in §3.6.
- Private-fetch cadence/batch-cap sizing for wedged subscribers (P4b) is a plan-level tuning
  detail, not specified here. The group-stall shape (one slow handler stalling its own priority
  group) is pre-warned in the bus's rustdoc (mod.rs:2036-2044); the bus-wide floor-starvation
  liveness bug this design fixes (§3.5) is new to this design, not something that rustdoc already
  covered.
- CLOUD-227 closure claim to be re-verified against that ticket's exact wording at review.
