# Spec 0029: `Aggregate::handle()` persist-before-publish ordering (Direction A)

**Issue:** Linear [CLOUD-242](https://linear.app/catallactical/issue/CLOUD-242) (`epoch: Aggregate::handle() publishes events before persisting state, breaking Inline-dispatch reentrant saga chains`)
**Status:** Ready — single decided direction (A). Full delivery plan in §8.
**Crate:** `epoch_core` owns the change (the `EventStoreBackend` trait split + the `Aggregate::handle()` reorder). `epoch_pg` and `epoch_mem` override the two new trait methods to actually split store from publish.
**Commit scope:** `fix(core)` (concept `event-store`).
**Discovery:** `specs/0029-cloud242-discovery.md` (see its **## Decision (resolved 2026-09-07)** — Directions B and C are dropped; this spec is A only).
**Anchored to:** current HEAD (`2049b95`). Line anchors below were re-read against source at this rev; the implementation plan must re-anchor before editing.
**Workspace version:** `0.1.0` (pre-1.0), no external SemVer obligation.

---

## 1. Problem

`epoch_core`'s generic `Aggregate::handle()` writes and **synchronously publishes**
events *before* it persists the aggregate's new state row. Confirmed ordering in the
success path (`epoch_core/src/aggregate.rs`):

1. `self.get_event_store().store_events(events)` — `aggregate.rs:505`.
2. `state_store.persist_state(...)` (or `delete_state`) — `aggregate.rs:515` /
   `:524`.

In both production backends, `store_events()` **fuses persist and publish**:

- `epoch_pg` (`epoch_pg/src/event_store.rs:521`): `begin()` → `store_events_in_tx`
  (`:527`) → `tx.commit()` → **`self.publish_events(stored_events)`** (`:530`,
  defined at `:200`). The whole Inline-subscriber walk runs inside step 1, before
  step 2.
- `epoch_mem` (`epoch_mem/src/event_store.rs:157`): validate → store under lock →
  drop lock → **Phase 3 `bus.publish(event)`** for every event (`:227`). Same fusion.

### 1.1 The real axis: top-level vs. nested dispatch (not same-bus vs. cross-bus)

The bug is **not** "same-bus fails, cross-bus is safe." The existing passing test
`inline_mode_cross_bus_cascade_sees_persisted_state_of_same_stream_command`
(`epoch_pg/tests/inline_dispatch_integration_tests.rs:730`) already contains a
**same-bus reentrant** `handle(Increment)` on the counter's own bus, and it
**succeeds** — because it runs as a *nested* dispatch, deferred by the Inline bus's
cross-bus queue until the outer unit of work drains (and thus until the triggering
command's `persist_state()` has run).

The broken shape is a **top-level** trigger with a **synchronous same-bus subscriber**:
a top-level `handle(Create)` whose own bus carries a saga that reacts to `Created` by
calling `handle(Increment)` on the same aggregate. Nothing defers that subscriber; it
runs synchronously inside step 1's publish, before step 2's `persist_state()`. The
nested `handle()` reads state at `aggregate.rs:408` (`state_store.get_state(state_id)`),
finds no row, and the aggregate's `handle_command` returns `NotFound`. That error
propagates back through the publish call, so the outer `store_events()` returns `Err`
and the outer `persist_state()` **never runs**.

### 1.2 The event-store/state-store durability split (finding beyond the ticket)

Because the outer `store_events()` returns `Err`, the outer `persist_state()` is
skipped, yet the event was already durably committed inside `store_events()` (pg:
`tx.commit()` at `:528` succeeded before publish; mem: inserted under lock before
Phase 3). Final state: a **durably committed `Created` event with no corresponding
state row.** Confirmed by the probe (§2). This is a genuine split, not a transient
read race a retry would resolve — the outer command aborted, so nothing retries it.

Under Direction A this window **inverts** (§4.3), and the spec resolves the inverted
window rather than leaving it open.

### 1.3 The already-correct precedent in the same file

`AggregateTransaction::handle()` / `commit()` (`epoch_core/src/aggregate.rs:996`
onward) already gets this right for the transactional API: `store_events_in_tx`
(`:996`) and `persist_state_in_tx` (`:1006`) run in one DB transaction, and
`publish_event` (`:1063`) is deferred until **after** commit. On publish failure it
returns `CommitError::Publish` (`:1070`), documented (`:1035`) as *"Event publishing
failed (but transaction is committed)"* — i.e. **events-durable / publish-failed is
non-fatal and recoverable by replay.** This is the precedent Direction A extends to
the plain path. **The fix must not regress this ordering.**

### 1.4 Why Directions B and C are dead (recorded, not re-litigated)

Per the discovery's Decision section, verified against current source:

- **C (defer same-bus reentrant command dispatch in the Inline bus) is dead.**
  `INLINE_CTX`/`InlineDispatchState` (`epoch_pg/src/event_bus/mod.rs`) only ever
  intercepts *publishes*. The reentrant failure happens inside `Aggregate::handle()`
  at the `get_state` read (`aggregate.rs:408`) **before any publish** — there is
  nothing for the bus queue to catch. C is also incompatible with catacloud's
  `EmailDeliverySaga::record_delivery` (`notifications/src/email_delivery_saga.rs:818-847`),
  which wraps the reentrant `.handle()` in a synchronous no-backoff `retry!` loop
  (`macros/src/lib.rs`) and propagates the final `Err` in-band through
  `Saga::handle_event` into the spec-0028 fail-closed/DLQ path. Deferring the nested
  command past the outer caller's return breaks that contract.
- **B (route plain `handle()` through `TransactionalAggregate`) is dead.**
  `TransactionalAggregate: Aggregate<Self::SupersetEvent>` makes "`handle()`'s default
  body delegates to `TransactionalAggregate`" a plausible supertrait cycle with no
  confirmed escape, and it would force ~13 in-repo `Aggregate` impls (only 2 currently
  implement `TransactionalAggregate`) plus all downstream ones to add the bound.

Neither is retained as an alternative.

---

## 2. Empirical grounding (probe)

A throwaway probe, `epoch_pg/tests/zz_probe_cloud242.rs`, is present in the worktree,
**untracked and not committed**. It is evidence only, **not** the regression test (it
prints rather than asserts and uses a `zz_probe_`-prefixed throwaway table). It
reproduces the top-level same-bus failure exactly:

- error `Event(BUSPublishError(InlineDispatchError(Command(NotFound))))` — the saga's
  `NotFound` from the nested `Increment`, wrapped by the inline-dispatch path, wrapped
  again as the outer `Create`'s `HandleCommandError::Event`;
- final DB state: `Created` durably committed in `epoch_events`, **no** counter state
  row (§1.2).

The probe is deleted in P4; its asserting replacement (T1/T2) lands in the tracked
suite (R5).

---

## 3. What must NOT break

- **`AggregateTransaction`'s ordering (§1.3).** Events + state written together,
  publish deferred until after commit. Unchanged; regression-pinned by the existing
  `epoch_pg`/`epoch_mem` transactional tests.
- **`DispatchMode::Async`.** `epoch_mem`'s `publish()` is a fire-and-forget channel
  send (`epoch_mem/src/event_store.rs:710` sends on `event_tx` and returns). `epoch_pg`'s
  `PgEventBus::publish()` under Async is a literal no-op (`epoch_pg/src/event_bus/mod.rs:3780`,
  `Box::pin(async { Ok(()) })`) — delivery instead comes from a DB trigger firing
  `NOTIFY` on the event `INSERT` at `tx.commit()` (`mod.rs:1310`), which happens
  inside the *persist* half of `store_events()`, not the `publish()` call. Neither
  path runs subscribers synchronously inside `store_events()`, so moving
  `publish_events()` to *after* `persist_state()` cannot make Async worse — but the
  reason is that Async delivery timing is **already decoupled from this reorder
  entirely** (pg's listener wakes off the trigger/commit, not off `publish()`;
  mem's channel send timing is unaffected by where the send call sits relative to
  `persist_state()`), not that it "narrows an existing race." Framing is **"no worse
  than before."** Pinned by T5 (§7).
- **The cross-bus cascade guarantee.**
  `inline_mode_cross_bus_cascade_sees_persisted_state_of_same_stream_command` must
  still pass unchanged (R6).
- **`store_events()`'s atomicity contract (spec 0020).** All-or-nothing persistence,
  publish-after-persist, empty-batch no-op — verified by
  `crate::testing::verify_store_events_atomicity`. The default `store_events()` keeps
  its exact current behaviour (§4.2), so this contract is untouched (R7).
- **No schema migration.** Ordering/dispatch defect, not a storage-shape change.
- **Every existing caller of `EventStoreBackend::store_events` stays byte-identical.**
  Only the single call site at `aggregate.rs:505` changes.

---

## 4. Design (Direction A)

### 4.1 Shape: a narrow additive `EventStoreBackend` split, publish re-issued by `handle()`

Direction A is **not** a two-line swap in `aggregate.rs` (persist and publish are
fused inside `store_events()`), and it is **not** a reorder of the fused
`store_events()`/`publish_events()` internals (that would perturb every caller). It is
a **narrow, additive** split at the trait boundary:

Add two methods to `EventStoreBackend` (`epoch_core/src/event_store.rs:24`), each with
a **default body that preserves today's behaviour**, so non-overriding backends are
byte-identical:

```rust
/// Durably persist `events` WITHOUT publishing them to the bus, returning the
/// stored events (backends that assign identifiers such as `global_sequence`
/// return the enriched copies) for a later `publish_events` call.
///
/// This is the persist half of the fused `store_events`. `Aggregate::handle()`
/// uses it to persist state BEFORE publishing, so a synchronous Inline
/// subscriber that reenters the same aggregate observes durable state.
///
/// # Default
/// Delegates to `store_events` (which persists AND publishes) and echoes the
/// input events back. Backends that do not override this keep today's
/// publish-before-persist ordering (acceptable: they are not on the reentrant
/// Inline write path). Production backends (pg, mem) override it to genuinely
/// split persist from publish.
async fn store_events_without_publish(
    &self,
    events: Vec<Event<Self::EventType>>,
) -> Result<Vec<Event<Self::EventType>>, Self::Error> {
    self.store_events(events.clone()).await?;
    Ok(events)
}

/// Publish already-durable `events` to the bus. Pairs with
/// `store_events_without_publish`.
///
/// # Default
/// No-op (`Ok(())`). Combined with the `store_events_without_publish` default —
/// which already published inside `store_events` — this keeps non-overriding
/// backends byte-identical.
async fn publish_events(
    &self,
    _events: Vec<Event<Self::EventType>>,
) -> Result<(), Self::Error> {
    Ok(())
}
```

Note the default `store_events_without_publish` takes `events.clone()` only in the
fallback path; the pg/mem overrides consume `events` without the clone.

> **Naming/shape is provisional** — the implementer confirms the final method names
> and whether `store_events_without_publish` returns the enriched `Vec` (pg needs the
> `global_sequence`-stamped copies for `publish_events`) after reading the trait at
> implementation time. The invariant is: (a) a persist-only entry point, (b) a
> separate publish entry point, (c) defaults that leave non-overriding backends
> byte-identical.
>
> **Naming collision to avoid:** `epoch_pg`'s `PgEventStore` already has an inherent
> `pub async fn publish_events` (`epoch_pg/src/event_store.rs:200`) with the same
> name/arity this trait method would use. Rust resolves an inherent method before a
> trait method at a `self.` call site, so this would compile without recursing — but
> it is a silent-resolution trap (the trait method and the inherent method could
> drift apart unnoticed). Name the new trait method distinctly, e.g.
> `publish_stored_events`, to avoid the collision outright.

### 4.2 `store_events` stays the fused default; only `handle()` changes ordering

`store_events` itself is **not** modified in `epoch_pg`/`epoch_mem` — it keeps calling
persist-then-publish exactly as today, so its atomicity contract (spec 0020) and every
other caller are untouched. Backends override the *two new* methods to share the
persist and publish halves; the existing `store_events` body can optionally be
re-expressed as `store_events_without_publish` + `publish_events` for DRY, but only if
that produces byte-identical behaviour (empty-batch no-op preserved).

The only ordering change is at `aggregate.rs:505`, which becomes:

```rust
let stored = self.get_event_store()
    .store_events_without_publish(events)
    .await
    .map_err(HandleCommandError::Event)?;

// ... persist_state / delete_state (unchanged block at :515 / :523) ...

self.get_event_store()
    .publish_events(stored)
    .await
    .map_err(HandleCommandError::Event)?;
```

`publish_events` is issued **after** the `persist_state`/`delete_state` +
`after_persist` block, on both the state-present and state-absent branches, so a
reentrant same-stream subscriber always sees durable state (or a durable delete). The
`events_applied` count and `after_persist` hook semantics are unchanged.

### 4.3 The inverted durability window, resolved (R4)

After the reorder the failure window inverts: `store_events_without_publish` succeeds
(event durable) → `persist_state` succeeds (state durable) → `publish_events` may
fail. That yields **durable event + durable state + unpublished event**, surfaced as
`HandleCommandError::Event`. This is **strictly better than today's split** (§1.2):
the state row now exists, and an unpublished-but-durable event is exactly the
condition the codebase already treats as non-fatal and replay-recoverable —
`store_events`' own rustdoc (`epoch_core/src/event_store.rs:88`, *"Publishing is
best-effort: a bus failure after the commit does not roll back the persisted
events"*) and `CommitError::Publish` in the transactional path (§1.3).

**Decision:** the plain path adopts the same contract. No new error variant is
introduced — `HandleCommandError::Event` already carries the store's error type, and a
publish failure returned from `publish_events` is documented on `handle()`'s rustdoc
as *"events and state are durable; publish failed and is recoverable by replay,"*
mirroring `CommitError::Publish`. For the **reentrant same-stream** case that motivated
this ticket, the nested `handle()` now runs after the outer `persist_state()`, sees
durable state, and **succeeds** — so `publish_events` returns `Ok` and no inverted
window occurs on the covered path. In-band, synchronous error propagation for
catacloud's `retry!` loop is preserved: a genuinely failing reentrant subscriber still
surfaces its `Err` synchronously through `publish_events` → `handle()`.

R4 is therefore **resolved on the covered path** (no durable event without its state
row) and the residual publish-failure window is **explicitly bounded and documented**
as identical to the already-accepted `CommitError::Publish` / best-effort-publish
contract — not a new or worse hazard.

### 4.4 Async is no worse than before

Under Async, `epoch_mem`'s `publish_events` is a non-blocking channel send
(`epoch_mem/src/event_store.rs:710`); `epoch_pg`'s Async delivery is driven by the
DB trigger's `NOTIFY` on `INSERT`, fired at `tx.commit()` inside the persist half of
`store_events()` — not by the `publish()`/`publish_events()` call at all (§3).
Moving `publish_events()` to after `persist_state()` therefore does not change pg's
Async delivery timing in any way, and only changes *when the call happens*, not
*what it does*, for mem. No Async guarantee is weakened. Pinned by T5.

---

## 5. Requirements

- **R1** After the fix, a **top-level** same-bus Inline saga that reacts to an event by
  invoking a command on the same aggregate observes the aggregate's persisted state and
  must not fail not-found on state the outer command is creating.
- **R2** `AggregateTransaction` ordering (§1.3) is unchanged and still passes its
  existing tests.
- **R3** `DispatchMode::Async` behaviour is no worse than before (§4.4),
  regression-pinned by a **named** test (T5), not a bare "byte-for-byte" assertion.
- **R4** The event-store/state-store durability split is **resolved** on the covered
  path and the residual inverted window is **documented** as the bounded, non-fatal
  `CommitError::Publish`-equivalent contract, in rustdoc on `handle()` and the two new
  trait methods (§4.3).
- **R5** A tracked, **asserting** regression test for the top-level same-bus reentrant
  case is added to `epoch_pg/tests/inline_dispatch_integration_tests.rs`, reusing the
  existing `CounterAggregate` fixture. `zz_probe_cloud242.rs` is removed.
- **R6** `inline_mode_cross_bus_cascade_sees_persisted_state_of_same_stream_command`
  still passes unchanged.
- **R7** No schema migration; `store_events()`'s atomicity contract (spec 0020) and
  every existing `store_events` caller are byte-identical; only `aggregate.rs:505`'s
  call site changes.
- **R8** Non-overriding `EventStoreBackend` implementors (test backends,
  downstream custom backends) compile and behave identically via the trait defaults
  (§4.1) — the new methods must not be required methods.

---

## 6. Behavioural risk owned

- **Inverted publish-failure window (§4.3).** Bounded and documented as equivalent to
  the existing best-effort-publish / `CommitError::Publish` contract; strictly better
  than today's durable-event-no-state split.
- **Trait surface growth.** Two new defaulted methods on `EventStoreBackend`. Additive,
  non-breaking (R8); production backends override, everyone else inherits today's
  behaviour.
- **Async ordering.** Reorder is in the safe direction (§4.4); explicitly pinned (T5)
  rather than assumed.

---

## 7. Test plan (paper)

Reuse the `CounterAggregate` / `CounterEvent` / `CounterCommand` / `CounterState`
fixture already in `epoch_pg/tests/inline_dispatch_integration_tests.rs`; DB-gated via
`common::try_get_pg_pool()`; `#[serial_test::serial]` per the file's conventions. No
absolute global-sequence assertions.

| # | Test | Setup sketch | Key assertions |
|---|---|---|---|
| T1 | Top-level same-bus reentrant sees persisted state (R1, R5) | `CounterAggregate` + Inline; `IncrementOnCreatedSaga` on the counter's **own** bus reacting to `Created` with `handle(Increment)`; **top-level** `handle(Create { value: 0, .. })` (no trigger bus, no cross-bus hop, matching the existing saga's `Create` payload at `:684`) | Outer `Create` returns `Ok`; final state row exists with **`value == 1`** (0 from `Create { value: 0 }` + 1 from `Increment`) and **`version == 2`** (`stream_version` starts at 1: Created=1, Incremented=2 — matches the cross-bus test's assertion at `:808`/`:811`); no `NotFound` |
| T2 | No event-store/state-store split (R4) | T1 setup | After the top-level call, the committed `Created` + `Incremented` events each have a corresponding state row; `version == 2` reconciles with two applied events |
| T3 | Cross-bus cascade unchanged (R6) | Existing `inline_mode_cross_bus_cascade_sees_persisted_state_of_same_stream_command` | Passes unchanged (`value == 1`, `version == 2`) |
| T4 | `AggregateTransaction` ordering unchanged (R2) | Existing transactional-path tests (`epoch_pg`/`epoch_mem`) | Pass unchanged |
| T5 | Async no worse than before (R3) | An **Async-mode** variant of T1's shape (same fixture, `DispatchMode::Async`), asserting the top-level `handle(Create)` returns `Ok` and, after the background listener drains, the reentrant increment is applied | Named pin `inline_dispatch_integration_tests::async_mode_same_bus_reentrant_still_delivers` (or equivalent); no synchronous failure, eventual `value == 1` / `version == 2` |
| T6 | Non-overriding backend unaffected (R8) | A minimal in-repo `EventStoreBackend` impl that only implements `store_events` (e.g. reuse the one in `epoch_core/tests/store_events_atomicity_contract_test.rs:185`) | Compiles without implementing the new methods; `store_events` behaviour byte-identical (atomicity contract test still green) |

---

## 8. Delivery plan (TDD)

Each phase: failing test → implement → refactor → gates.

1. **P1 — red (regression tests).** Add T1 and T2 to
   `epoch_pg/tests/inline_dispatch_integration_tests.rs` (top-level same-bus
   reentrant, plus the no-split reconciliation). They fail today exactly as the probe
   shows (`NotFound` / missing state row). Add T5 (Async variant) — passes today, held
   as the Async pin. Files: `epoch_pg/tests/inline_dispatch_integration_tests.rs`.
2. **P2 — green (trait split, defaults).** Add `store_events_without_publish` and
   `publish_events` to `EventStoreBackend` (`epoch_core/src/event_store.rs`) with the
   behaviour-preserving defaults (§4.1). Confirm the workspace still builds and T6's
   non-overriding backend needs no change (R8). Files:
   `epoch_core/src/event_store.rs`.
3. **P3 — green (backend overrides).** Override the two methods in
   `epoch_pg/src/event_store.rs` (persist via `store_events_in_tx`+`commit`, publish
   via existing `publish_events` inherent method) and `epoch_mem/src/event_store.rs`
   (Phase 1/2 persist, Phase 3 publish). Keep each backend's existing `store_events`
   body byte-identical (or re-express as the two halves only if provably identical,
   empty-batch no-op preserved). Files: `epoch_pg/src/event_store.rs`,
   `epoch_mem/src/event_store.rs`.
4. **P4 — green (reorder `handle()`).** Change `aggregate.rs:505` to
   `store_events_without_publish` → `persist_state`/`delete_state` + `after_persist` →
   `publish_events` (§4.2) on both branches. T1/T2 now pass; T3/T4/T5/T6 stay green.
   Update `handle()` and the two trait methods' rustdoc with the R4 window contract
   (§4.3). Files: `epoch_core/src/aggregate.rs` (+ rustdoc).
5. **P5 — cleanup + gates.** Remove `epoch_pg/tests/zz_probe_cloud242.rs`. Run
   `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, and the full suite
   (DB-gated where a database is available). No `unwrap()`/`expect()` outside tests; no
   schema migration.

---

## 9. Acceptance criteria

- [ ] **AC-1** T1/T2 (top-level same-bus reentrant + no split) tracked in
  `epoch_pg/tests/inline_dispatch_integration_tests.rs` and green with `value == 1`,
  `version == 2` (R1, R4, R5).
- [ ] **AC-2** T3/T4/T5/T6 green — cross-bus, transactional, Async, and non-overriding
  backend all unchanged (R2, R3, R6, R8).
- [ ] **AC-3** The two additive `EventStoreBackend` methods exist with
  behaviour-preserving defaults; `store_events`' atomicity contract and every existing
  caller are byte-identical; only `aggregate.rs:505`'s call site changed (R7).
- [ ] **AC-4** R4 satisfied: split resolved on the covered path; inverted window
  documented as the bounded `CommitError::Publish`-equivalent contract in rustdoc on
  `handle()` and the two new methods.
- [ ] **AC-5** `zz_probe_cloud242.rs` removed; its asserting equivalent lives in the
  tracked suite.
- [ ] **AC-6** `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, full
  `cargo test` (DB-gated where available) clean; no `unwrap()`/`expect()` outside
  tests; no schema migration.

---

## 10. Out of scope

- Catacloud's downstream workaround
  (`web-admin/tests/api_integration/api_integration_annotation_review_notifications.rs`)
  — tracked in catacloud; this fix should make it removable, but the removal is not
  this repo's change.
- Directions B and C (dropped per the discovery Decision; not retained as follow-ups).
- Any change to `DispatchMode::Async`'s dispatch mechanics beyond regression-pinning.

---

## 11. Residual uncertainty

- Final names/return shape of the two trait methods (§4.1) — an implementation detail
  the implementer confirms against the trait at HEAD; the invariant (persist-only +
  publish + behaviour-preserving defaults) is fixed.
- Whether pg's `publish_events` inherent method (`epoch_pg/src/event_store.rs:200`) is
  reused directly or wrapped by the trait override — either is fine provided the
  returned enriched (`global_sequence`-stamped) events are what gets published.

---

## Phases (JSON)

```json
{
  "spec": "0029-cloud242-inline-dispatch-reentrant-persist-ordering",
  "commit_scope": "fix(core)",
  "concept": "event-store",
  "direction": "A",
  "phases": [
    {
      "id": "P1",
      "name": "Red: top-level same-bus reentrant regression tests",
      "kind": "test",
      "files": ["epoch_pg/tests/inline_dispatch_integration_tests.rs"],
      "tests": ["T1", "T2", "T5"],
      "expect": "T1/T2 fail today (NotFound + missing state row); T5 passes and pins Async"
    },
    {
      "id": "P2",
      "name": "Green: additive EventStoreBackend trait methods with behaviour-preserving defaults",
      "kind": "impl",
      "files": ["epoch_core/src/event_store.rs"],
      "tests": ["T6"],
      "expect": "workspace builds; non-overriding backends unchanged via defaults"
    },
    {
      "id": "P3",
      "name": "Green: pg and mem backend overrides split persist from publish",
      "kind": "impl",
      "files": ["epoch_pg/src/event_store.rs", "epoch_mem/src/event_store.rs"],
      "tests": [],
      "expect": "existing store_events behaviour byte-identical; new methods genuinely split"
    },
    {
      "id": "P4",
      "name": "Green: reorder Aggregate::handle() to persist-before-publish + R4 rustdoc",
      "kind": "impl",
      "files": ["epoch_core/src/aggregate.rs"],
      "tests": ["T1", "T2", "T3", "T4", "T5", "T6"],
      "expect": "T1/T2 pass; T3-T6 stay green; inverted window documented"
    },
    {
      "id": "P5",
      "name": "Cleanup + gates",
      "kind": "chore",
      "files": ["epoch_pg/tests/zz_probe_cloud242.rs"],
      "tests": [],
      "expect": "probe removed; fmt + clippy -D warnings + full DB-gated suite clean"
    }
  ]
}
```
