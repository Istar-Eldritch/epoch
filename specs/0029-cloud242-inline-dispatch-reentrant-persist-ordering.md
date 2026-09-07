# Spec 0029: `Aggregate::handle()` persist-before-publish ordering (Direction A)

**Issue:** Linear [CLOUD-242](https://linear.app/catallactical/issue/CLOUD-242) — `Aggregate::handle()` published events before persisting state, breaking Inline-dispatch reentrant saga chains.
**Status:** Shipped. Delivered across 4184d2d, 3d0272c, 78fad6d, dcb1dfd, 9c58fcd on branch `cloud-242`.
**Crate:** `epoch_core` owns the change (the `EventStoreBackend` trait split + the `Aggregate::handle()` reorder); `epoch_pg` and `epoch_mem` override the two new methods to genuinely split store from publish.
**Discovery / fuller history:** `specs/0029-cloud242-discovery.md`.

---

## 1. Problem

`Aggregate::handle()` wrote and **synchronously published** events *before* it persisted
the aggregate's new state row. In both production backends `store_events()` fused persist
and publish (pg: `tx.commit()` then `publish_events`; mem: store under lock then Phase-3
`bus.publish`), so the whole Inline-subscriber walk ran inside the store step, before
`persist_state()`.

**The real axis is top-level vs. nested dispatch, not same-bus vs. cross-bus.** A *nested*
same-bus reentrant `handle()` already succeeded (the Inline bus defers it via its cross-bus
queue until the outer unit of work drains). The broken shape was a **top-level** trigger
with a **synchronous same-bus subscriber**: a top-level `handle(Create)` whose own bus
carries a saga that reacts to `Created` by calling `handle(Increment)` on the same
aggregate. Nothing defers it; it ran synchronously inside the publish, before
`persist_state()`. The nested `handle()` read state via `get_state`, found no row, and
returned `NotFound` — which propagated back so the outer `store_events()` returned `Err`
and the outer `persist_state()` **never ran**.

**Durability split (finding beyond the ticket):** the event was already durably committed
inside `store_events()` while the skipped `persist_state()` left no state row — a durable
`Created` event with no state row, not a transient read race (nothing retries the aborted
outer command). Direction A **inverts** this window and resolves it (§4.3).

**Precedent:** `AggregateTransaction::handle()`/`commit()` already got this right — events +
state in one transaction, publish deferred until after commit, publish failure surfaced as
the non-fatal, replay-recoverable `CommitError::Publish`. Direction A extends that contract
to the plain path.

---

## 2. Why Direction A (pre-implementation history worth keeping)

An initial draft proposed three candidate directions (A/B/C). Review **unanimously blocked**
it: the root cause was framed wrong (as same-bus vs. cross-bus), and two directions were
non-viable. An interview grounded in a downstream consumer's actual code — catacloud's
`EmailDeliverySaga::record_delivery`, which wraps the reentrant `.handle()` in a synchronous
no-backoff `retry!` loop and propagates the final `Err` in-band through `Saga::handle_event`
into the spec-0028 fail-closed/DLQ path — resolved the fork in favour of **Direction A only**.
The spec was rewritten and re-reviewed (OK with notes) before implementation began.

Directions B and C were dropped, not retained as follow-ups:

- **C (defer same-bus reentrant dispatch in the Inline bus)** — the bus only intercepts
  *publishes*; the reentrant failure happens at the `get_state` read *before any publish*,
  so there is nothing to catch. Also incompatible with catacloud's synchronous in-band
  `retry!` contract above.
- **B (route plain `handle()` through `TransactionalAggregate`)** — plausible supertrait
  cycle with no confirmed escape, and would force the `TransactionalAggregate` bound onto
  ~13 in-repo `Aggregate` impls (only 2 have it) plus all downstream ones.

---

## 3. Design (as shipped)

**A narrow, additive `EventStoreBackend` split, publish re-issued by `handle()`.** Two new
methods were added to `EventStoreBackend`, each with a default body preserving today's
behaviour so non-overriding backends stay byte-identical:

- `store_events_without_publish(events) -> Vec<Event>` — persist half; default delegates to
  `store_events` (which persists **and** publishes) and echoes the events back.
- `publish_stored_events(events)` — publish half; default is a no-op `Ok(())`. Combined with
  the persist default (which already published), non-overriding backends are unchanged.

The publish method is named `publish_stored_events` (not `publish_events`) to avoid the
silent-resolution trap with `PgEventStore`'s existing inherent `publish_events`.

`store_events` itself is unchanged in pg/mem — it keeps persist-then-publish, so its
spec-0020 atomicity contract and every other caller are byte-identical. pg/mem override only
the two new methods (pg: `store_events_in_tx`+`commit`, then the inherent `publish_events`
on the enriched `global_sequence`-stamped events; mem: Phase 1/2 persist, Phase 3 publish).

**The only ordering change is in `Aggregate::handle()`:** `store_events_without_publish` →
`persist_state`/`delete_state` + `after_persist` → `publish_stored_events`, issued after the
persist block on **both** the state-present and state-absent branches
(`epoch_core/src/aggregate.rs`). A reentrant same-stream subscriber now always sees durable
state (or a durable delete).

### 3.1 Inverted durability window — resolved (R4)

After the reorder the window inverts to **durable event + durable state + possibly-unpublished
event**, surfaced as `HandleCommandError::Event`. That is strictly better than the old
durable-event-no-state split, and an unpublished-but-durable event is exactly what the
codebase already treats as non-fatal and replay-recoverable. No new error variant. This
contract is documented in rustdoc on `Aggregate::handle()` — **verified shipped** at
`epoch_core/src/aggregate.rs` (the numbered persist-then-publish steps 6/7/8 plus the
"events and state are already durable; only the publish failed, and it is recoverable by
replay … mirrors `CommitError::Publish`" paragraph). For the reentrant same-stream case that
motivated the ticket, the nested `handle()` now runs after the outer `persist_state()`,
succeeds, and no inverted window occurs on the covered path. Synchronous in-band `Err`
propagation for catacloud's `retry!` loop is preserved.

### 3.2 Async is no worse than before

pg's Async delivery is driven by the DB trigger's `NOTIFY` at `tx.commit()` (inside the
persist half), not by the publish call; mem's Async publish is a non-blocking channel send.
Moving publish after `persist_state()` changes *when the call happens*, not *what it does* —
no Async guarantee weakened. Pinned by T5.

---

## 4. Requirements (all met)

- **R1** Top-level same-bus Inline reentrant command observes persisted state; no false `NotFound`.
- **R2** `AggregateTransaction` ordering unchanged; existing tests pass.
- **R3** `DispatchMode::Async` no worse than before, pinned by a named test (T5).
- **R4** Durability split resolved on the covered path; inverted window documented in rustdoc
  on `handle()` and the two new methods as the bounded `CommitError::Publish`-equivalent contract.
- **R5** Tracked, asserting regression test added to
  `epoch_pg/tests/inline_dispatch_integration_tests.rs`; throwaway probe removed.
- **R6** `inline_mode_cross_bus_cascade_sees_persisted_state_of_same_stream_command` still passes.
- **R7** No schema migration; `store_events` atomicity contract (spec 0020) and every existing
  caller byte-identical; only the `handle()` call site changed.
- **R8** Non-overriding `EventStoreBackend` implementors compile and behave identically via the
  defaults — the new methods are not required.

---

## 5. Tests

Reuse the `CounterAggregate` fixture in
`epoch_pg/tests/inline_dispatch_integration_tests.rs`; DB-gated via
`common::try_get_pg_pool()`, `#[serial_test::serial]`; no absolute global-sequence assertions.

| # | Test | Assertion |
|---|---|---|
| T1 | Top-level same-bus reentrant sees persisted state (R1, R5) | outer `Create` `Ok`; state row `value == 1`, `version == 2`; no `NotFound` |
| T2 | No event-store/state-store split (R4) | committed `Created` + `Incremented` each have a state row; `version == 2` |
| T3 | Cross-bus cascade unchanged (R6) | existing test passes unchanged |
| T4 | `AggregateTransaction` ordering unchanged (R2) | existing transactional tests pass |
| T5 | Async no worse (R3) | Async-mode variant of T1; `Ok`, eventual `value == 1` / `version == 2` |
| T6 | Non-overriding backend unaffected (R8) | minimal `store_events`-only backend compiles; atomicity contract green |

---

## 6. Delivery (shipped, TDD)

1. **P1 — red** (`4184d2d`): T1/T2 added (fail today), T5 added as Async pin.
   `epoch_pg/tests/inline_dispatch_integration_tests.rs`.
2. **P2 — green** (`3d0272c`): additive `store_events_without_publish` / `publish_stored_events`
   on `EventStoreBackend` with behaviour-preserving defaults. `epoch_core/src/event_store.rs`.
3. **P3 — green** (`78fad6d`): pg + mem override the two methods, splitting persist from publish;
   `store_events` bodies byte-identical. `epoch_pg/src/event_store.rs`, `epoch_mem/src/event_store.rs`.
4. **P4 — green, load-bearing fix** (`dcb1dfd`): reorder `Aggregate::handle()` to
   persist-before-publish on both branches + R4 rustdoc. `epoch_core/src/aggregate.rs`.
5. **P5 — cleanup + gates** (`9c58fcd`): remove throwaway probe, clippy fix; fmt + clippy
   `-D warnings` + full DB-gated suite clean.

---

## 7. Out of scope

- Catacloud's downstream workaround removal (tracked in catacloud).
- Directions B and C (dropped, not follow-ups).
- Any change to Async dispatch mechanics beyond regression-pinning.
