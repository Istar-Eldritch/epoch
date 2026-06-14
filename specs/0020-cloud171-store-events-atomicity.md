# Spec 0020: `epoch_core` — Enforce Atomic `store_events()` Batch Writes

**Issue:** Linear [CLOUD-171](https://linear.app/catallactical/issue/CLOUD-171/epoch-core-store-events-default-impl-is-non-atomic-enforce-batch)
**Status:** Proposed (ready for review)
**Created:** 2026-06-14
**Crate:** `epoch_core` (with knock-on dev-dependency wiring in `epoch_mem` / `epoch_pg`)
**Commit scope:** `feat(core)!` / concept scope `event-store`
**Discovery:** `specs/0020-cloud171-discovery.md`
**Workspace version:** `0.1.0` (pre-1.0 — breaking changes are permitted with a clear migration note)

---

## 1. Problem Statement

The default implementation of `EventStoreBackend::store_events()` in
`epoch_core/src/event_store.rs:65-78` loops over `store_event()` per event:

```rust
async fn store_events(&self, events: Vec<Event<Self::EventType>>) -> Result<(), Self::Error> {
    for event in events {
        self.store_event(event).await?;
    }
    Ok(())
}
```

This is **not atomic**. If any event in the batch fails (the most common cause being
an optimistic-concurrency `stream_version` mismatch mid-batch), the events before the
failure have already been persisted and published, while the events after it have not.
The result is a **partially-written batch** — a corrupted aggregate history that
violates the all-or-nothing invariant every event-sourced system depends on.

The two first-party backends already override this method correctly:

- **`epoch_mem/src/event_store.rs:162-238`** — a two-phase approach under a single
  `Mutex` guard: validate every event's `stream_version` first, then commit all
  events, then publish after releasing the lock. Storage is atomic; publishing is
  best-effort post-commit (acceptable for event sourcing — see §6).
- **`epoch_pg/src/event_store.rs:424-435`** — a real PostgreSQL transaction: `BEGIN`,
  `store_events_in_tx()`, `COMMIT`, then `publish_events()`. Truly atomic on the
  storage side.

The danger is entirely in the **default**: a third-party backend that does not
override `store_events()` silently inherits the broken loop, and nothing in the type
system, the docs, or the test suite warns the implementor. The current doc comment
even acknowledges the non-atomicity in prose — which is precisely the trap: it
documents a footgun instead of removing it.

There is **no reusable contract test** a backend author can run to prove their
`store_events()` is atomic.

---

## 2. Goals and Non-Goals

### 2.1 Goals

- **G-1** Make it impossible to *silently* ship a non-atomic `store_events()`: no
  backend should inherit a broken batch write without making an explicit decision.
- **G-2** State the atomicity contract for `store_events()` explicitly in the trait
  documentation (Acceptance Criterion 1).
- **G-3** Provide a **reusable contract test** that any backend — first- or
  third-party — can run to verify its `store_events()` is atomic (Acceptance
  Criterion 2).
- **G-4** Keep both first-party backends (`epoch_mem`, `epoch_pg`) passing the new
  contract test with **no behavioral change** — they are already atomic.

### 2.2 Non-Goals

- **NG-1** Changing the publish-after-commit semantics. Best-effort publishing after
  durable persistence is the established and correct event-sourcing behavior
  (projections catch up by replay). This spec does not touch it.
- **NG-2** Adding cross-stream / multi-aggregate transactional guarantees beyond what
  each backend already offers.
- **NG-3** Changing the `store_event()` (single-event) signature or semantics.
- **NG-4** Adding a new persisted column, migration, or wire-format change.

---

## 3. Decision

The discovery listed three options:

1. **Remove the default impl** — breaking, forces an explicit decision per backend.
2. **Keep default + document + add contract test helper** — non-breaking, but the
   broken default still exists.
3. **Add `stream_version` contiguity validation** — catches one bug class, does not
   solve atomicity.

**Chosen: a combination of (1) and the test helper from (2).**

- **Remove the default implementation** of `store_events()` (G-1). Because the
  workspace is pre-1.0 (`0.1.0`), this breaking change is acceptable, and it is the
  only option that prevents silent inheritance of the footgun. After removal,
  `store_events()` becomes a required trait method; an implementor must write *some*
  body and is thereby forced to confront atomicity.
- **Removal alone is insufficient** — a forced implementation can still be written as
  a non-atomic loop. The actual guarantee comes from a **reusable contract test**
  (G-3) that exercises a mid-batch failure and asserts nothing was persisted. Backend
  authors wire this test into their own suites.
- **Document the contract** on the trait method (G-2).

Option 3 (contiguity validation) is explicitly rejected as the *primary* mechanism:
it is a partial fix and would live in backends anyway. The contract test already
covers the contiguity-mismatch scenario as its failure trigger.

> **Reviewer decision point:** if breaking the trait is undesirable even at 0.1.0, the
> fallback is to *keep* the default but change its body to **unconditionally fail**
> (e.g. `unimplemented!`-style error) — but that defers the failure to runtime rather
> than compile time and is strictly worse than removal. Removal is recommended.

---

## 4. The Atomicity Contract

`store_events()` must guarantee, for a single call with a batch of events:

1. **All-or-nothing persistence.** Either every event in the batch is durably
   persisted, or none of them are. A partial failure (e.g. an optimistic-concurrency
   conflict on the *k*-th event) must leave the store in the state it had **before**
   the call — no event from the batch is visible to a subsequent `read_events()`.
2. **Per-stream version monotonicity preserved.** A batch that would violate
   `stream_version` contiguity for any stream it touches must be rejected in full.
3. **Publish-after-persist.** Events are published to the bus only after they are
   durably persisted. Publishing is best-effort and may be partial on bus failure;
   this does **not** roll back the persisted events (see NG-1 / §6).
4. **Empty batch is a no-op.** `store_events(vec![])` returns `Ok(())` and persists
   nothing.

---

## 5. Proposed Changes

### 5.1 `epoch_core/src/event_store.rs` — Make `store_events()` a required method

Remove the default body and replace the doc comment with an explicit contract.

```rust
/// Appends multiple events to one or more streams **atomically**.
///
/// # Atomicity Contract
///
/// Implementations **must** guarantee all-or-nothing persistence:
///
/// - If every event is persisted successfully, the call returns `Ok(())`.
/// - If **any** event fails to persist (most commonly a `stream_version`
///   optimistic-concurrency conflict mid-batch), the call returns `Err(..)` and
///   **no event from the batch is persisted** — a subsequent [`read_events`]
///   must observe the store exactly as it was before this call.
/// - Per-stream `stream_version` monotonicity must be preserved; a batch that
///   would create a gap or conflict for any stream must be rejected in full.
/// - An empty batch is a no-op that returns `Ok(())`.
///
/// Events are published to the event bus **only after** successful, durable
/// persistence. Publishing is best-effort: a bus failure after the commit does
/// **not** roll back the persisted events (projections recover by replay).
///
/// # Verifying Your Implementation
///
/// Use [`crate::testing::verify_store_events_atomicity`] in your backend's test
/// suite to assert this contract holds.
///
/// [`read_events`]: Self::read_events
async fn store_events(&self, events: Vec<Event<Self::EventType>>) -> Result<(), Self::Error>;
```

Note: removing `async fn ... { ... }` and leaving `async fn ...;` makes it a required
method (the trait already uses `#[async_trait]`).

### 5.2 `epoch_core` — New feature-gated `testing` module with the contract helper

Add a `testing` feature and module exposing a reusable, backend-agnostic contract
test. It is feature-gated so it never ships in production builds.

**`epoch_core/Cargo.toml`:**

```toml
[features]
# Test-only utilities (contract tests) for verifying backend implementations.
testing = []
```

**`epoch_core/src/lib.rs`:**

```rust
#[cfg(feature = "testing")]
pub mod testing;
```

**`epoch_core/src/testing.rs`** (new file):

```rust
//! Reusable contract tests for backend implementations.
//!
//! Gated behind the `testing` feature. Backend authors add
//! `epoch_core = { path = "..", features = ["testing"] }` to their `[dev-dependencies]`
//! and call these helpers from their own test suite.

use crate::event::Event;
use crate::event_store::EventStoreBackend;
use uuid::Uuid;

/// Asserts that a backend's [`EventStoreBackend::store_events`] honours the
/// all-or-nothing atomicity contract (see the trait docs).
///
/// The caller supplies:
/// - a freshly-constructed, empty `backend`, and
/// - `make_event(stream_id, stream_version)` producing a valid event for the
///   backend's `EventType`.
///
/// The helper:
/// 1. seeds version 1 with a single `store_event`,
/// 2. submits a batch `[v2, v3, v5]` whose third event skips version 4,
/// 3. asserts the batch is rejected (`Err`), and
/// 4. asserts the stream still contains exactly the one seeded event — proving
///    no event from the failed batch was persisted.
///
/// It additionally verifies the empty-batch no-op and a fully-valid batch.
///
/// # Panics
///
/// Panics with a descriptive message on any contract violation, so it reads as a
/// drop-in `#[tokio::test]` body.
pub async fn verify_store_events_atomicity<B>(
    backend: B,
    make_event: impl Fn(Uuid, u64) -> Event<B::EventType>,
) where
    B: EventStoreBackend,
{
    use tokio_stream::StreamExt;

    // --- empty batch is a no-op ---
    backend
        .store_events(Vec::new())
        .await
        .expect("empty batch must return Ok");

    // --- mid-batch failure must persist nothing from the batch ---
    let stream_id = Uuid::new_v4();
    backend
        .store_event(make_event(stream_id, 1))
        .await
        .expect("seeding version 1 must succeed");

    let bad_batch = vec![
        make_event(stream_id, 2), // ok
        make_event(stream_id, 3), // ok
        make_event(stream_id, 5), // gap: skips 4 -> must abort whole batch
    ];
    let result = backend.store_events(bad_batch).await;
    assert!(
        result.is_err(),
        "store_events with a mid-batch version gap must return Err"
    );

    let mut stream = backend
        .read_events(stream_id)
        .await
        .expect("read_events must succeed");
    let mut count = 0usize;
    while let Some(item) = stream.next().await {
        item.expect("stream item must not error");
        count += 1;
    }
    assert_eq!(
        count, 1,
        "after a rejected batch the stream must contain only the seeded event \
         (non-atomic backend leaked partial writes)"
    );

    // --- a fully-valid batch is persisted in full ---
    let good_batch = vec![make_event(stream_id, 2), make_event(stream_id, 3)];
    backend
        .store_events(good_batch)
        .await
        .expect("valid batch must succeed");

    let mut stream = backend.read_events(stream_id).await.unwrap();
    let mut count = 0usize;
    while let Some(item) = stream.next().await {
        item.unwrap();
        count += 1;
    }
    assert_eq!(count, 3, "valid batch must append all events");
}
```

> The contract test induces the partial failure via a **`stream_version` gap**, which
> every backend that enforces optimistic concurrency (both first-party backends do)
> will reject. This is the realistic, deterministic failure mode for batch writes and
> needs no fault injection.

### 5.3 `epoch_mem` — Wire the contract test in

Add the feature-gated dev-dependency and a test that runs the helper against
`InMemoryEventStore`.

**`epoch_mem/Cargo.toml`:**

```toml
[dev-dependencies]
epoch_core = { path = "../epoch_core", features = ["testing"] }
# (plus the existing dev-dependencies)
```

**`epoch_mem/tests/store_events_atomicity_tests.rs`** (new):

```rust
// builds an InMemoryEventStore + a make_event closure, then:
epoch_core::testing::verify_store_events_atomicity(store, |stream_id, version| {
    // Event::<MyEventData>::builder() ... build().unwrap()
}).await;
```

The existing in-file unit tests in `epoch_mem/src/event_store.rs`
(`..._store_events_version_mismatch_aborts_all`, `..._store_events_batch`,
`..._store_events_empty_batch`, `..._store_events_multiple_streams`) are retained as
backend-specific coverage; the new helper provides the *portable* contract.

### 5.4 `epoch_pg` — Wire the contract test in

**`epoch_pg/Cargo.toml`** `[dev-dependencies]`: add `features = ["testing"]` to the
existing `epoch_core` dependency path (currently `epoch_pg` pulls `epoch_core`
transitively via `epoch_mem`; add an explicit dev-dependency on `epoch_core` with the
`testing` feature).

Add a `#[serial_test::serial]`, DB-gated integration test (matching the existing
`epoch_pg` integration-test harness) that constructs a `PgEventStore` against a clean
table and calls `verify_store_events_atomicity`.

### 5.5 `epoch_core/src/lib.rs` — exports

No change to `prelude` (the `testing` module is intentionally **not** in the prelude —
it is opt-in via the feature flag and referenced by full path).

---

## 6. Publishing Semantics (unchanged, restated)

Both backends publish **after** durable persistence and treat publish failure as
non-fatal to persistence:

- `epoch_mem` drops the `Mutex` guard, then publishes; a publish error returns
  `Err(PublishEvent)` but the events remain stored.
- `epoch_pg` commits the transaction, then `publish_events()`; a publish error returns
  after the commit.

This spec does **not** alter this (NG-1). The contract test asserts only **persistence
atomicity** (via `read_events`), never publish atomicity, so it is correct for both
backends.

---

## 7. TDD Implementation Plan

Following the repository TDD convention (red → green → refactor):

1. **(red)** Add `epoch_core/src/testing.rs` with `verify_store_events_atomicity` and
   the `testing` feature in `Cargo.toml` + `lib.rs`. Add the `epoch_mem` integration
   test that calls it. Run `cargo test -p epoch_mem` — it passes (mem is already
   atomic), confirming the helper is correctly wired.
2. **(red→proof)** Temporarily revert `epoch_mem`'s override to the naive loop and
   confirm the helper **fails** (proving it actually catches non-atomicity), then
   restore the override. *(Manual verification step; not committed.)*
3. **(green)** Remove the default body from `EventStoreBackend::store_events()` and
   update the trait docs (§5.1). Run `cargo build -p epoch_core` — confirm it fails
   *only* for any backend lacking an override (both first-party backends already
   override, so the workspace still builds).
4. **(green)** Add the `epoch_pg` DB-gated contract test (§5.4).
5. **(refactor)** Confirm `cargo fmt`, `cargo clippy --all-targets -- -D warnings`,
   and the full `cargo test` (plus DB-gated `epoch_pg` tests where a database is
   available) are clean.
6. **Docs:** Add a CHANGELOG / migration note for the breaking trait change (§9).

---

## 8. Acceptance Criteria

- [ ] **AC-1** The `EventStoreBackend::store_events()` trait docs state the atomicity
  contract explicitly (all-or-nothing persistence, version monotonicity,
  publish-after-persist, empty-batch no-op).
- [ ] **AC-2** `store_events()` has **no default implementation** — it is a required
  method; a backend that omits it fails to compile.
- [ ] **AC-3** A reusable, backend-agnostic contract test
  (`epoch_core::testing::verify_store_events_atomicity`) exists behind the `testing`
  feature and fails for a non-atomic (loop-based) `store_events()`.
- [ ] **AC-4** `epoch_mem` and `epoch_pg` each run the contract test and pass with no
  behavioral change.
- [ ] **AC-5** `cargo test` (workspace, DB-gated tests where available),
  `cargo fmt --check`, and `cargo clippy --all-targets -- -D warnings` are clean.

---

## 9. Breaking Change & Migration Notes

`feat(core)!: require explicit atomic store_events() implementation`

`EventStoreBackend::store_events()` no longer has a default implementation. Any
third-party backend that previously relied on the (non-atomic) default must now
provide its own implementation.

**Migration for downstream implementors:**

- If your backend supports transactions, wrap the inserts in one transaction and
  publish after commit (see `PgEventStore::store_events`).
- If your backend is in-memory or lock-based, validate all versions first, then commit
  all, then publish (see `InMemoryEventStore::store_events`).
- Verify with `epoch_core::testing::verify_store_events_atomicity` (enable the
  `testing` feature in your `[dev-dependencies]`).

A copy-paste-safe *temporary* shim that reproduces the old behavior (explicitly
opting in to non-atomicity) can be the old loop body — but this is discouraged and
will fail the contract test.

---

## 10. Files Touched

| File | Change |
|------|--------|
| `epoch_core/src/event_store.rs` | Remove `store_events()` default body; rewrite doc with atomicity contract |
| `epoch_core/src/testing.rs` | **New** — `verify_store_events_atomicity` contract helper |
| `epoch_core/src/lib.rs` | `#[cfg(feature = "testing")] pub mod testing;` |
| `epoch_core/Cargo.toml` | Add `[features] testing = []` |
| `epoch_mem/Cargo.toml` | Dev-dependency `epoch_core` with `features = ["testing"]` |
| `epoch_mem/tests/store_events_atomicity_tests.rs` | **New** — runs the contract test |
| `epoch_pg/Cargo.toml` | Dev-dependency `epoch_core` with `features = ["testing"]` |
| `epoch_pg/tests/` | **New** DB-gated test running the contract test |
| `CHANGELOG` | Breaking-change entry (§9) |
