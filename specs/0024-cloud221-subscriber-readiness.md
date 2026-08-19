# Spec 0024: `epoch_pg` subscriber readiness + startup safety

**Issue:** CLOUD-221 · **Status:** Implemented · **Crates:** `epoch_core`, `epoch_pg`
**Scope:** `feat(core)` / `feat(pg)` · additive only, no migration, no schema change.
**Follow-on:** cross-bus quiescence (originally R3) split to **spec 0025**.
**Forcing consumer:** catacloud `integration/src/lib.rs::initialize_aggregates` (nine `PgEventBus` + raw-SQL `PolicyGraph` warm-up). catacloud is **not** changed here; warm-up deletion is blocked on 0025.

---

## 1. Problem

catacloud warms its in-memory `PolicyGraph` with a raw-SQL scan of `policy_events` because the bus can't (a) report when a subscriber is caught up, (b) express a projection that replays from zero every boot, or (c) wait for a cross-bus saga cascade to settle. This is the CLOUD-217 stale-read incident class.

This spec delivers (a) and (b). (c) — the cross-bus quiescence gate — is unimplementable on the current listener (a bus-level `fillable_head` can't see per-subscriber historical fence state) and is split to **spec 0025**. Because warm-up completeness depends on (c), **0024 alone does not let catacloud delete the warm-up**.

Relevant existing behaviour: `subscribe()` already does synchronous catch-up before going live (current as of its own call); `start_listener()` spawns a task that does **no** catch-up before its first `tokio::select!` (delivery waits on NOTIFY or the 1 s tick); `publish()` is a no-op in `DispatchMode::Async` (delivery via AFTER INSERT trigger); `fast_forward_all_subscribers()` parks every checkpoint at head, which suppresses replay for a from-zero projection.

## 2. Goals / Non-Goals

**Goals:** R1 `subscriber_lag` + `wait_until_caught_up` (per-subscriber and bus-wide); R2 one catch-up pass in `start_listener` before its wait loop; R4 make trigger-less Async subscription impossible in the common path (implicit trigger) and loudly reported otherwise; R5 `ReplayAlways` subscriber kind (replay from zero every start, in-memory HWM readiness, same gate as checkpointed); G-6 additive, byte-for-byte identical for opt-out observers.

**Non-Goals:** CLOUD-222 sentinel watermark; catacloud changes; any persisted state (HWM is per-process by design); multi-instance/Coordinated readiness; changes to delivery semantics or gap/fence resolution; **cross-bus quiescence (R3 → spec 0025)**.

## 3. Key Grounding Facts

- Readiness methods are **PG-specific** (head query + in-memory HWM) → inherent methods on `PgEventBus`, not on the cross-backend `EventBus` trait.
- `EventObserver` has a defaulted `priority()`; add defaulted `subscription_mode()` the same way — default keeps all observers `Checkpointed`.
- **Wrapper forwarding is load-bearing (D2):** `ProjectionHandler`'s `EventObserver` impl forwards only `on_event`, so a bare defaulted method is unreachable for a wrapped projection (would resolve `Checkpointed`, yielding an empty graph). Mirror the `priority()` precedent: add `subscription_mode()` to the `Projection` and `Saga` traits and forward from `ProjectionHandler`, `SagaHandler`, `SagaAdapter`, and the `impl Saga for Arc<S>` blanket. The forcing consumer subscribes **wrapped** (`ProjectionHandler::new(...)`).
- Listener seeds `SubscriberState` from the checkpoint **table** (`Ok(None) => 0`); a `ReplayAlways` subscriber has no row → naive seed re-delivers all history. Seeding must read the **HWM** instead (Correction 2).
- Reuse `subscribe()`'s existing catch-up for R2/R5; factor a `head_sequence()` helper out of `fast_forward_all_subscribers`; `subscriber_lag` = `head − position`.

## 4. Design

### 4.1 R1 — lag + readiness (inherent on `PgEventBus`)

```rust
pub async fn head_sequence(&self) -> Result<Option<u64>, PgEventBusError>;
pub async fn subscriber_lag(&self, subscriber_id: &str) -> Result<u64, PgEventBusError>;
pub async fn wait_until_caught_up(&self, subscriber_id: &str, timeout: Duration) -> Result<bool, PgEventBusError>;
pub async fn wait_until_all_caught_up(&self, timeout: Duration) -> Result<bool, PgEventBusError>;
```

- `subscriber_lag` = `head − position`, saturating; `position` dispatches on mode (§4.6).
- `wait_until_caught_up` snapshots `target = head_sequence()` **once** and polls `position >= target`; it does **not** chase a growing head (that's 0025). `Ok(true)` caught-up, `Ok(false)` timeout, `Err` only on genuine DB error.
- Poll cadence: small fixed interval (default 25 ms, a `pub(crate) const`), not the 1 s `flush_interval`. R2 makes the gate resolve as soon as first-batch processing completes.
- **Mode resolution (R-6):** id-only signatures look the id up in the live `projections` registry to read its `subscription_mode()`. `Checkpointed` → DB checkpoint; `ReplayAlways` → in-memory HWM; **not registered → explicit unknown-subscriber `Err`**, never a silent `Checkpointed` fallback (which could read a fast-forwarded checkpoint and report false-ready — Correction 4).
- **Hazard:** this is a **local** readiness check. It is unsafe as a startup gate for any consumer whose sagas cascade across buses (settles over an incomplete model = CLOUD-217 false-ready). Cross-bus readiness needs spec 0025.

### 4.2 R2 — pre-loop catch-up on `start_listener`

Refactor `subscribe()`'s inline catch-up into a reusable `pub(crate) async fn catch_up_from_checkpoint(...)`. At the top of the spawned task, before the loop, run it once over every registered subscriber (respecting the priority sort). Closes the `[subscribe() resolves, listener first batch]` window by direct query instead of NOTIFY/timer tick. Same at-least-once discipline (transient errors retried; listener still starts). Prerequisite for R1 being cheap.

### 4.3 R3 — cross-bus quiescence: split to spec 0025

Unimplementable on the current listener: the gap fence is per-subscriber, historical, and lives in a `SubscriberState` stack-local inside the listener task; the move-out/move-back ownership model hides every actively-working subscriber's state. Safe exposure needs a listener concurrency-model change none of R1/R2/R4/R5 require. **Consequence:** 0024's only startup gate is the local `wait_until_caught_up`, unsafe for the incident's cross-bus cascade → warm-up deletion blocked on 0025.

### 4.4 R4 — Async trigger-absence hazard

1. **Implicit trigger (primary):** `start_listener()` (Async) probes `trigger_exists()` and calls `ensure_trigger()` only when absent, before the R2 pass; events written pre-trigger are recovered by the R2 pass regardless of NOTIFY. A failure here (missing DDL rights, or the existence probe itself failing) is logged via `warn!` and does not fail `start_listener()` — the timer tick + R2 pass keep delivery correct without it. Explicit `setup_trigger()` callers are unaffected and keep the unconditional drop+create, hard-failing on error, for a caller that deliberately wants to force a rebind.
2. **Loud report (defence-in-depth):** `subscribe()` in Async probes `pg_trigger` and emits a `WARN` naming bus + channel if absent.

No hard error — the timer fallback + R2 pass keep delivery correct.

### 4.5 R5 — `ReplayAlways` subscriber kind

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum SubscriptionMode {
    #[default] Checkpointed,
    ReplayAlways,
}
```

Defaulted `subscription_mode()` on `EventObserver`, **also** on the `Projection`/`Saga` traits and forwarded through all wrappers + the `Arc<S>` blanket (D2). `PgEventBus` gains `hwm: Arc<Mutex<HashMap<String, u64>>>`.

`subscribe()` when `ReplayAlways`: catch up from `0` ignoring any persisted checkpoint; route advancement to `hwm[id]`, seeded with the sequence reached at end of catch-up; never write `epoch_event_bus_checkpoints`. Listener + R2 pass advance the HWM, not the DB. `fast_forward_all_subscribers` **skips** `ReplayAlways`.

- **Correction 2 (seeding):** listener seeding site must read `hwm[id]` for `ReplayAlways`, not the checkpoint table, else the first live batch re-delivers all history.
- **Correction 3 (re-subscribe):** a fresh `subscribe()` resets the HWM to 0 at the start of catch-up, then re-seeds as it progresses (model is genuinely fresh).
- **Correction 3 revised (listener restart):** `shutdown()` only signals/awaits the task; the `projections` registry, HWM map, and in-memory models **survive** in-process. Replaying from 0 into a surviving `apply_to_graph`-style model corrupts it. So on `shutdown()` + `start_listener()`, catch up **from the surviving HWM**, not from 0.

### 4.6 R5 × R1 (load-bearing)

Readiness "position" dispatches on `subscription_mode`: `Checkpointed` → `get_checkpoint(id)`; `ReplayAlways` → `hwm[id]`. Defining R1 against checkpoints alone would leave every `ReplayAlways` subscriber (the exact incident subscriber) un-gateable. Unregistered id → explicit `Err` (Correction 4). Hard test target `test_replay_always_readiness_via_hwm`.

### 4.7 Forcing consumer

**Enabled here:** `PolicyGraphProjection` becomes a normal `ReplayAlways` subscriber registered **wrapped** in `ProjectionHandler` (mode reaches the bus only via D2 forwarding), plus HWM readiness to gate it. **Blocked on 0025:** deleting the raw-SQL warm-up — the policy graph's completeness depends on the `iam/compute → policy_events → policy_bus` cascade draining, which a per-subscriber gate can't guarantee (reproduces CLOUD-217). Preconditions P1/P2 live with the cross-bus gate in 0025.

## 5. Requirements

| ID | Requirement |
|----|-------------|
| R-1 | `head_sequence`, `subscriber_lag`, `wait_until_caught_up`, `wait_until_all_caught_up` on `PgEventBus`; snapshot target, sub-tick poll, `bool` return |
| R-2 | `start_listener` pre-loop catch-up over all subscribers; shared `catch_up_from_checkpoint` |
| R-3 | **Removed → spec 0025** (IDs kept for traceability) |
| R-4 | Async `start_listener` idempotent `ensure_trigger`; `subscribe` WARNs on absent trigger; no hard error |
| R-5 | `SubscriptionMode` defaulted on `EventObserver`/`Projection`/`Saga` + forwarded (D2); `ReplayAlways` replays from 0, no checkpoint, per-process HWM; seed from HWM (Corr. 2); reset on re-subscribe, catch up from surviving HWM on restart (Corr. 3); `fast_forward` skips it |
| R-6 | Position dispatches on `subscription_mode`; id-only signatures resolve via registered observer; unknown id → explicit `Err` (Corr. 4) |
| R-7 | Byte-for-byte backward compat; existing suites pass unmodified |
| R-8 | Additive only; clippy `-D warnings` + fmt clean; full `cargo test` green |

## 6. Test Plan

**`epoch_core` unit:** `subscription_mode_defaults_to_checkpointed`; `subscription_mode_forwarded_through_wrappers` (D2, projection + saga wrappers).

**`epoch_pg` integration** (`#[serial]`, `#[tokio::test]`, fresh `Uuid`s):

| Test | Maps to |
|------|---------|
| `test_subscriber_lag_reports_behind` | R-1 |
| `test_wait_until_caught_up_gates_on_head` | R-1, R-2 |
| `test_wait_until_caught_up_times_out` | R-1 |
| `test_start_listener_catches_up_before_loop` | R-2 |
| `test_ensure_trigger_implicit_in_start_listener` | R-4 |
| `test_subscribe_warns_when_trigger_absent` | R-4 |
| `test_replay_always_replays_from_zero` | R-5 |
| `test_replay_always_listener_does_not_redeliver_history` | R-5 (Corr. 2) |
| `test_replay_always_hwm_reset_on_resubscribe` | R-5 (Corr. 3) |
| `test_replay_always_listener_restart_from_hwm` | R-5 (Corr. 3 revised) |
| `test_fast_forward_skips_replay_always` | R-5 |
| `test_replay_always_readiness_via_hwm` | R-6 |
| `test_readiness_unknown_subscriber_errors` | R-6 (Corr. 4) |
| `test_no_config_subscriber_identical_baseline` | R-7 |

Cross-bus quiescence tests move to spec 0025. Regression: `cargo test --workspace` green unmodified; clippy `-D warnings`; fmt `--check`.

## 7. Failure Modes

- **Moving head:** `wait_until_caught_up` snapshots target, won't hang; convergence is 0025's job.
- **Burned/in-flight tail** (spec 0019 non-transactional `nextval`): raw head may need `timeout > gap_timeout` to resolve via the gap backstop.
- **Timeout:** returns `bool`, not `Err`; caller decides fatality.
- **`DispatchMode::Inline`:** no listener; readiness methods return `Err(PgEventBusError::InlineDispatchNotSupported)` rather than silently reporting not-ready forever (Inline dispatch tracks no checkpoint or HWM position); R2/R4 no-op.
- **`InstanceMode::Coordinated`:** subscriber owned by another instance not gated locally (NG-4).
- **HWM process-local by design:** crash → next boot replays from 0 (intended); in-process restart catches up from surviving HWM (no reset).

## 8. Files Changed

| File | Change |
|------|--------|
| `epoch_core/src/event_store.rs` | `SubscriptionMode` enum; defaulted `subscription_mode()` on `EventObserver` |
| `epoch_core/src/projection.rs` | Defaulted `subscription_mode()` on `Projection`; forward from `ProjectionHandler` |
| `epoch_core/src/saga.rs` | Defaulted on `Saga`; forward from `SagaHandler`, `SagaAdapter`, `Arc<S>` blanket |
| `epoch_core/src/lib.rs` | Re-export `SubscriptionMode` (module + prelude) |
| `epoch_pg/src/event_bus/mod.rs` | `hwm` field; readiness methods; `catch_up_from_checkpoint` refactor; R2 pass + `ensure_trigger`; Async WARN; `ReplayAlways` routing incl. seeding site + HWM reset; `fast_forward` skip |
| `epoch_pg/src/lib.rs` | Re-export `SubscriptionMode` |
| `epoch_pg/tests/pgeventbus_integration_tests.rs`, `tests/common/mod.rs` | §6.2 tests |
| `CHANGELOG.md` | Unreleased `Added` |

No new dependencies. No migration.

## 9. Breaking / Dependency Notes

Non-breaking additive (`feat(core)`/`feat(pg)`, no `!`). External implementors compile unchanged. HWM per-process by design. catacloud follow-up (warm-up deletion + `ReplayAlways` + 0025 gate) out of scope. CLOUD-222 sentinel is independent defence-in-depth.

## 10. Phased Delivery (as implemented)

| Phase | Focus | Commits |
|-------|-------|---------|
| 1 | `epoch_core` `SubscriptionMode` + wrapper forwarding (D2) | `21e15db5`, `49159a4d` |
| 2 | R2 `catch_up_from_checkpoint` refactor + pre-loop pass; R4 implicit trigger + WARN | `a30bd783` (+ `04c8660a` fmt) |
| 3 | R5 `ReplayAlways`: `hwm` field, seeding from HWM (Corr. 2), reset/restart (Corr. 3), `fast_forward` skip | `0894df4f` |
| 4 | R1 readiness methods, kind-dispatched position (R6), unknown-subscriber `Err` (Corr. 4) | `6915fb6f`, `9b233f19`, `181cbd42` |
| 5 | Hygiene: fmt, clippy `-D warnings`, full test; error propagation in readiness methods; CHANGELOG | `585d340f`, `4ddfce12`, `6c8d04bc` |

## 11. Acceptance Criteria

| ID | Criterion |
|----|-----------|
| AC-1 | Await "subscriber at head" with timeout, gate readiness on it (R1) |
| AC-2 | No fixed ~1 s cost on healthy boot (pre-loop catch-up, R2) |
| AC-3 | Async-without-trigger impossible in normal path or loudly reported (R4) |
| AC-4 | In-memory replay-always projection expressible without bus bypass, same readiness gate (R5+R1) |
| AC-5 | Warm-up deletable **only after spec 0025 lands**; 0024's per-subscriber gate alone reproduces CLOUD-217 |
| AC-6 | Additive; existing suite passes unmodified; clippy/fmt clean |

## 12. Open Questions

- **OQ-1:** readiness methods inherent on `PgEventBus` (need PG head query + HWM); `epoch_mem` analogue deferred.
- **OQ-2:** poll interval a `const` for now; promote to config only if a consumer needs tuning.
- **OQ-3:** Coordinated cross-instance readiness left to a horizontal-scaling ticket.
- **OQ-4 (out of scope):** the reused `subscribe()` catch-up advances a linear `PendingCheckpoint` with **no** gap fence, so over `1,2,3,[gap 4],5` it flushes `checkpoint = 5` and a late event 4 is skipped. Pre-existing hazard, merely propagated into listener startup here; fix under a separate ticket (route catch-up through `advance_contiguous_checkpoint`).
