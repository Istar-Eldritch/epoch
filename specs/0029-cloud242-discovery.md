# CLOUD-242 Discovery: `Aggregate::handle()` publishes before persisting state

## Source

Linear ticket CLOUD-242 (`epoch: Aggregate::handle() publishes events before
persisting state, breaking Inline-dispatch reentrant saga chains`), pinned
against rev `2049b95`. Full ticket text fetched via `linear issue view
CLOUD-242` and reproduced below, followed by empirical verification done in
this session.

### Ticket text

> `epoch`'s generic `Aggregate::handle()` (in `epoch_core/src/aggregate.rs`)
> writes events and synchronously publishes them to the event bus *before*
> persisting the aggregate's new state row. Under `DispatchMode::Inline`,
> this means any subscriber (saga or projection) that reacts to the
> just-published event by calling `.handle()` again on the *same* aggregate
> — before the outer call returns — reads a state row that does not exist
> yet, and fails with a not-found error.
>
> Discovered while implementing CLOUD-220 (converting notification DAO
> integration tests from a shared live server to `TestApp`/`DispatchMode::
> Inline`). Blocks 15 of that ticket's 19 target tests: any notification
> delivery test where `EmailDeliverySaga`/`TelegramDeliverySaga` records a
> delivery attempt by calling back into the `NotificationAggregate` that just
> emitted `NotificationCreated`.
>
> **Root cause** (traced against pinned rev `2049b95`): `epoch_core/src/
> aggregate.rs`'s generic `handle()`:
>
> 1. Reads current state from the state store (no event-replay fallback if
>    the row doesn't exist yet)
> 2. Computes new state via `apply()`
> 3. Calls `event_store.store_events()` — which (in `epoch_pg`) commits the
>    event-store transaction, then **synchronously calls**
>    `publish_events()`, walking every Inline subscriber (projections then
>    sagas) before returning
> 4. **Only after** `store_events()` **returns** does `handle()` call
>    `state_store.persist_state()` to write the new state row
>
> So a saga that reacts to the event from step 3 by invoking a new command on
> the *same* aggregate reads a state row that won't exist until step 4, which
> hasn't happened yet in the call stack.
>
> **Severity**: production is unaffected — production uses `DispatchMode::
> Async`, where `publish()` is deferred to a background listener task that
> runs strictly after the triggering `handle()` call (and its
> `persist_state`) has already returned. This is a dormant defect under
> Async, not a live production bug. It blocks Inline-dispatch test isolation
> for any self-referential or cross-bus saga chain, and has already caused a
> silent workaround elsewhere (`web-admin/tests/api_integration/
> api_integration_annotation_review_notifications.rs` in catacloud asserts
> against the raw `notification_events` table instead of the
> `notifications` projection table, with a comment describing this exact
> limitation).
>
> **Suggested direction** (not prescribed): reorder `handle()` to persist
> state before publishing, if that ordering is safe with respect to epoch's
> existing failure/rollback semantics (events are already committed by the
> time publish runs, so this needs care around what happens if
> `persist_state` fails after publish has already fired subscribers).
> Alternatively, guard against or document the reentrant-Inline-dispatch
> pattern explicitly if reordering has other correctness implications.
>
> **Blocks**: CLOUD-220 Phase 2 (test isolation onto TestApp/Inline dispatch)
> for 15 of 19 target tests.

## Codebase grounding

- `epoch_core/src/aggregate.rs` — `Aggregate::handle()` (the buggy
  non-transactional path). Order confirmed by reading: `store_events()` is
  called, then (only in the success path, and only if `state.is_some()`)
  `persist_state()` is called afterward. There is a second, *correct* code
  path in the same file: `AggregateTransaction::handle()` /
  `AggregateTransaction::commit()`, which already stores events and persists
  state within a DB transaction and defers `publish_event()` entirely until
  after `commit()` — i.e. state is guaranteed durable before any subscriber
  runs. This is the transactional/`TransactionalAggregate` API, a separate
  entry point from the plain `Aggregate::handle()` under investigation. Any
  fix should not regress this already-correct ordering.
- `epoch_pg/src/event_store.rs::PgEventStore::store_events()` (around
  line 521): begins a tx, calls `store_events_in_tx`, `tx.commit()`, then
  `self.publish_events(stored_events)` — confirms publish happens
  synchronously inside `store_events()`, before `Aggregate::handle()`'s own
  subsequent `persist_state()` call.
- `epoch_pg/src/event_bus/mod.rs` — `DispatchMode::Inline` re-entrancy
  handling (`INLINE_CTX`, `InlineDispatchState`, ~line 1067 onward). Existing
  design already distinguishes:
  - **same-bus re-entrant publish** (a publish triggered while that same bus
    is already draining on this task): queued FIFO, not immediately
    dispatched — this prevents publish-side recursion/deadlock, but does
    **not** defer the *command handling* that triggered the publish.
  - **cross-bus cascade** (a publish to a different bus than the one
    currently draining): deferred until the *outer* bus's queue fully
    drains, specifically to guarantee (per code comments, doc-item 5 in
    `epoch_pg/tests/inline_dispatch_integration_tests.rs`) that a cross-bus
    saga never observes a same-stream command's state before that command's
    `persist_state()` has run.
- `epoch_pg/tests/inline_dispatch_integration_tests.rs` — existing Inline
  dispatch regression suite (818 lines, 6 tests). Notably
  `inline_mode_cross_bus_cascade_sees_persisted_state_of_same_stream_command`
  (~line 728) already proves the cross-bus case is safe (uses a
  `CounterAggregate`/`CounterEvent`/`CounterCommand`/`CounterState` fixture,
  a `CreateOnFireSaga` on a separate `trigger_bus`, and an
  `IncrementOnCreatedSaga` on the counter's own `counter_bus`). This test
  currently passes. There is **no existing test** for the same-bus case
  (saga and aggregate sharing one bus, saga invoked as part of the
  *top-level*, non-reentrant publish of the triggering command).

## Empirical verification (probe)

A throwaway probe (`epoch_pg/tests/zz_probe_cloud242.rs`, untracked, left in
the worktree for reference, not committed) was written and run against a
disposable Postgres database to confirm the exact same-bus reentrant failure
mode the ticket describes but that the existing suite doesn't cover.

**Setup**: `CounterAggregate` (same fixture shape as
`inline_dispatch_integration_tests.rs`) with `DispatchMode::Inline`. A saga
(`IncrementOnCreatedSaga`-equivalent) subscribed directly to the counter's
*own* bus, reacting to `CounterEvent::Created` by calling
`counter.handle(Increment)` on the same aggregate. Top-level call:
`counter.handle(Create)` — no separate trigger bus, no cross-bus hop.

**Result**: reproduces exactly as the ticket describes, plus one detail the
ticket didn't state.

Call order (DEBUG logs):

```
top-level: handling Create
  Handling command (Create)
  Retrieving state -> None (expected, first command)
  Storing 1 events -> INSERT INTO epoch_events ... COMMIT   <- publish fires here, synchronously, before persist_state
  saga: handling Increment (nested, same bus, mid-outer-handle)
    Handling command (Increment)
    Retrieving state -> 0 rows returned (state row was never written — persist_state for Create hasn't run yet)
top-level Create returned Err(...)
```

Exact error: `Event(BUSPublishError(InlineDispatchError(Command(NotFound))))`
— the saga's `CounterError::NotFound` from `Increment`, wrapped by the
inline-dispatch error path, wrapped again as the outer `Create` command's own
`HandleCommandError`.

**Finding beyond the ticket's description**: the outer `Create` command's
`handle()` call aborts entirely (`store_events()` returns `Err` because the
nested saga's error propagates back through the publish call), so
`persist_state()` for `Create` **never runs**. Final DB state check confirms:
`zz_probe_counter_states` has **no row** for the counter, despite
`epoch_events` having a durably committed `Created` event (`INSERT ...
COMMIT` succeeded before the saga ran). This is an event-store/state-store
split — a durably published event with no corresponding aggregate state
row — not just a transient read race that resolves once retried. Any fix
needs to reason explicitly about this failure path: what should happen to an
already-committed event if `persist_state()` (or, under a reordered
design, something that runs after it) subsequently fails or is skipped.

## Problem framing for the fix

Two known-good reference points already exist in the codebase and should
anchor the design:

1. `AggregateTransaction::handle()`/`commit()` — the transactional API
   already gets this right: events + state are written together (same DB
   transaction), and publish is deferred until after commit. This is
   evidence the desired invariant ("no subscriber observes state before
   it's durable") is already an established epoch design principle, just
   not honored by the plain `Aggregate::handle()` path.
2. The Inline bus's existing cross-bus deferral — evidence that "defer
   dispatch of reentrant/cascading calls until the triggering unit of work
   is durable" is already the bus's own strategy for one axis (cross-bus);
   the gap is that same-bus reentrant dispatch runs the subscriber
   synchronously with no equivalent deferral relative to `persist_state()`.

Candidate directions (not prescribed — this is a decision for spec review,
per the ticket's own "needs design, not prescribed" framing):

- **(A) Reorder `Aggregate::handle()`** to persist state before publishing.
  Requires deciding what happens if `persist_state()` fails or the event
  publish itself fails after persist succeeds — inverse of today's dangling
  problem (durable event, no state) becomes a durable state row whose
  event hasn't published yet if publish fails, which existing `Publish`-vs-
  `Event` error variants and the transactional path's `CommitError::Publish`
  precedent (events already-durable, publish failure is non-fatal /
  recoverable via replay) may or may not extend cleanly to the plain path.
- **(B) Route the non-transactional `Aggregate::handle()` through the same
  store-then-persist-then-publish sequence the transactional path already
  uses**, effectively making `handle()` a thin wrapper over
  `begin()`/`handle()`/`commit()` for a single command. Would unify the two
  code paths and eliminate the divergent ordering outright, at the cost of
  requiring `Aggregate` implementors to also implement
  `TransactionalAggregate` (currently optional).
- **(C) Extend the Inline bus's existing deferral mechanism** to also defer
  same-bus reentrant *command dispatch* (not just publish) until the
  triggering command's full `handle()` (including `persist_state`) has
  returned — mirroring the cross-bus behavior already proven correct by
  `inline_mode_cross_bus_cascade_sees_persisted_state_of_same_stream_command`.
  This is Inline-bus-local and wouldn't change `Aggregate::handle()`'s
  ordering or Async-mode behavior at all, but does nothing for any caller
  that isn't going through the Inline bus's dispatch machinery (e.g. a
  test or tool calling `.handle()` directly without going through
  `publish()`'s reentrancy tracking) — needs verification of whether that
  gap matters in practice.

## Decision (resolved 2026-09-07, after review + interview)

Round-1 parallel review (3 fresh-context `reviewer` agents against the draft spec
`specs/0029-cloud242-inline-dispatch-reentrant-persist-ordering.md`) found the
spec's root-cause framing was wrong and, as a direct consequence, that Direction
C does not exist as a buildable mechanism:

- The real axis of the bug is **top-level vs. nested** dispatch, not **same-bus
  vs. cross-bus**. The existing passing test
  (`inline_mode_cross_bus_cascade_sees_persisted_state_of_same_stream_command`)
  already contains a same-bus reentrant command dispatch that succeeds, because
  it runs as a *nested* call (deferred by the cross-bus queue machinery), not a
  top-level one.
- `InlineDispatchState`/`INLINE_CTX` only ever intercepts *publishes*. A saga's
  reentrant `.handle()` call fails inside `Aggregate::handle()`
  (`epoch_core/src/aggregate.rs`, at the `get_state` read) before any publish
  happens — there is nothing for the bus's queue to catch. Direction C's
  "Inline-bus-local, doesn't touch `aggregate.rs`" claim is false; any real fix
  under that label still has to touch `epoch_core`.
- Direction C's deferral model is also incompatible with the actual downstream
  use case. catacloud's `EmailDeliverySaga::record_delivery`
  (`notifications/src/email_delivery_saga.rs:818-847` in the catacloud repo)
  wraps the reentrant `self.notification.handle(command)` call in a **tight
  synchronous retry loop** (`macros/src/lib.rs`'s `retry!`: no backoff, no async
  sleep, immediate in-band retry up to 5 attempts) and propagates the final
  `Err` out of the `Saga::handle_event` trait method into the existing
  fail-closed/DLQ error path (spec 0028). A design that defers the nested
  command past the outer caller's return breaks this: there is nothing left to
  retry synchronously, and errors could no longer propagate in-band the way the
  existing saga code (and spec 0028's Inline fail-closed contract) requires.
- Direction B's blast-radius costing ("every `Aggregate` implementor must also
  implement `TransactionalAggregate`") undersold a deeper problem: reviewers
  found `TransactionalAggregate: Aggregate<Self::SupersetEvent>` makes
  expressing "`handle()`'s default body delegates to `TransactionalAggregate`"
  a plausible supertrait cycle with no confirmed escape — B's feasibility as
  described is unverified, possibly not expressible at all.
- Direction A's blast radius was overstated in the draft spec ("touches the
  core write path used by every aggregate and both dispatch modes"). The real
  call graph has exactly one production caller of
  `EventStoreBackend::store_events` outside the mem-backend transactional path
  (`epoch_core/src/aggregate.rs:505`), and a narrow, additive
  `EventStoreBackend` method (a non-publishing store variant, publish issued
  separately by `handle()` after `persist_state()`) leaves every existing
  caller and both dispatch modes behaviorally unchanged.

**Decision: drop Directions B and C. The spec is to be rewritten around
Direction A only** — reorder `Aggregate::handle()` to persist state before
publishing, via the narrow additive `EventStoreBackend` method split (not a
reorder of the fused `store_events()`/`publish_events()` call), preserving
synchronous, in-band error propagation for reentrant same-stream commands.
This is the only candidate that matches catacloud's actual saga code, and the
only one whose mechanism is confirmed to exist. The event-store/state-store
durability split (§1.2 above) must still be resolved or explicitly bounded
under Direction A's own new failure window (durable state, unpublished event) —
see the review findings for the shape of that window and the precedent
(`CommitError::Publish` in the transactional path) to evaluate against it.

## Scope signal

- Directly blocks CLOUD-220 Phase 2 (15 of 19 target tests).
- Confirmed dormant-only in production (`DispatchMode::Async` unaffected).
- Existing regression coverage: `epoch_pg/tests/
  inline_dispatch_integration_tests.rs` (cross-bus case, passing) — a same-bus
  equivalent test is needed and does not yet exist in the tracked test suite
  (only the throwaway, untracked probe).
- A structurally identical pre-existing workaround exists downstream in
  catacloud's `web-admin/tests/api_integration/
  api_integration_annotation_review_notifications.rs`, described in the
  ticket; out of scope for this repo's spec but worth a spec note since a fix
  here should make that workaround removable (tracked separately in
  catacloud, not this repo).
