# Spec 0022: `epoch` — `read_events_range(from, to)` bounded-replay primitive

**Issue:** Linear [CLOUD-183](https://linear.app/catallactical/issue/CLOUD-183)
**Title:** epoch: add `read_events_range(from, to)` as the bounded-replay primitive
**Status:** Proposed (ready for review)
**Created:** 2026-06-21
**Ticket state:** Triage, Priority High, Unassigned
**Discovery:** `specs/0022-cloud183-discovery.md`
**Crates:** `epoch_core` (trait + new primitive + defaults), `epoch_pg` (impl), `epoch_mem` (impl)
**Commit scope:** `feat(core)!` / concept scope `event-store`
**Workspace version:** `0.1.0` (pre-1.0 — breaking trait changes permitted with a migration note)

---

## 1. Problem Statement

`EventStoreBackend` (`epoch_core/src/event_store.rs:25`) exposes two read primitives:

- `read_events(stream_id)` — full stream (`epoch_core/src/event_store.rs:33`).
- `read_events_since(stream_id, version)` — lower-bounded only
  (`epoch_core/src/event_store.rs:39`).

There is **no upper-bounded read**. A caller that needs the stream *up to* a specific
`stream_version` (Sentinel's `evaluate_at` / `scope_at` time-travel, plus any audit /
debugging / retroactive-reconstruction tooling) must read the whole stream and filter in
user-space, paying full I/O cost regardless of how early the cutoff is.

At the storage layer the upper bound is a free indexed predicate: every backend already
maintains a unique `(stream_id, stream_version)` index
(`epoch_pg/src/migrations/m001_create_events_table.rs:41`), so `WHERE stream_version <= $n`
is sargable, and the in-memory stream can simply stop yielding.

## 2. Goals / Non-Goals

### Goals

- Add a single bounded-replay primitive `read_events_range(stream_id, from, to)` to
  `EventStoreBackend`, with inclusive `Option<u64>` bounds (`[from, to]`, `None` = unbounded
  on that side).
- Make `read_events` and `read_events_since` **default trait implementations** that delegate
  to `read_events_range` — they stop being per-backend overrides.
- Implement `read_events_range` in `epoch_pg` (`PgEventStore`) and `epoch_mem`
  (`InMemoryEventStore`), pushing the upper bound down to storage.
- Preserve existing behaviour exactly; the existing test suite passes **unmodified**.
- Add tests for the new `to` bound (closed and half-open ranges) for both backends.

### Non-Goals (from ticket)

- Sentinel's `evaluate_at` / `scope_at` (the *consumer* — tracked separately).
- Any change to the event schema, event bus, projections, or aggregates.
- New dependencies; new migrations (the `UNIQUE (stream_id, stream_version)` index already
  exists).
- A shared `epoch_core::testing` parity helper for range reads (considered — see §8); kept
  out of scope to match the ticket's per-backend test instruction.

## 3. Current-State Anchors

### 3.1 `epoch_core`

- Trait `EventStoreBackend` — `epoch_core/src/event_store.rs:25`.
- `read_events` signature returning
  `Pin<Box<dyn EventStream<Self::EventType, Self::Error> + Send + 'life0>>` —
  `epoch_core/src/event_store.rs:33` (note the `async_trait`-generated `'life0` lifetime;
  the new method must use the same form).
- `read_events_since` signature — `epoch_core/src/event_store.rs:39`.
- `EventStream` trait — `epoch_core/src/event_store.rs:15`.
- `read_last_event` is already a defaulted method that drains `read_events`
  (`epoch_core/src/event_store.rs:~95`) — confirms the defaults-in-trait pattern is already
  in use and the `'life0`/`poll_fn` idiom is precedented.

### 3.2 `epoch_pg` (`PgEventStore`)

- `impl EventStoreBackend for PgEventStore` — `epoch_pg/src/event_store.rs:~388`.
- `read_events` delegates to `read_events_since(stream_id, 0)` —
  `epoch_pg/src/event_store.rs:397` (the `0` lower bound means "all rows": `>= 0`).
- `read_events_since` builds SQL
  `... WHERE stream_id = $1 AND stream_version >= $2 ORDER BY stream_version ASC`, binds
  `version as i64`, streams rows via `try_stream!` and `pg_db_event_to_event` —
  `epoch_pg/src/event_store.rs:405`.
- `pg_db_event_to_event` is the shared row→`Event` conversion (handles upcasting,
  dead-letter skip via `Ok(None)`) — `epoch_pg/src/event_store.rs:~260`.
- `UNIQUE (stream_id, stream_version)` index — `epoch_pg/src/migrations/m001_create_events_table.rs:41`.

### 3.3 `epoch_mem` (`InMemoryEventStore`)

- `impl EventStoreBackend for InMemoryEventStore` — `epoch_mem/src/event_store.rs:66`.
- `read_events` delegates to `read_events_since(stream_id, 1)` —
  `epoch_mem/src/event_store.rs:74`.
- `read_events_since` constructs
  `InMemoryEventStoreStream::new(data, stream_id, version - 1)` —
  `epoch_mem/src/event_store.rs:83` (**note the `version - 1`**: `version` is treated as a
  1-based `stream_version`, mapped to a 0-based Vec index `current_index`).
- `InMemoryEventStoreStream` struct — `epoch_mem/src/event_store.rs:320`; `new` sets
  `current_index = start_version as usize` — `epoch_mem/src/event_store.rs:337`.
- `poll_next` walks the `stream_events` Vec by `current_index` (an *index*, not a
  `stream_version` comparison) — `epoch_mem/src/event_store.rs:356`.
- Existing tests: `in_memory_event_store_read_events` (`epoch_mem/src/event_store.rs:927`),
  `in_memory_event_store_read_events_since` (`epoch_mem/src/event_store.rs:949`).
- Stored events are 1-based contiguous (`store_event` starts version at `1` and requires
  `event.stream_version == current`), so Vec index `i` always holds `stream_version == i + 1`.

## 4. Design

### 4.1 New trait primitive (`epoch_core`)

Add to `EventStoreBackend`:

```rust
/// Fetches an inclusive `[from, to]` `stream_version` range from the backend.
///
/// `from`/`to` are inclusive `stream_version` bounds; `None` is unbounded on that side.
/// Events are yielded in ascending `stream_version` order.
async fn read_events_range(
    &self,
    stream_id: Uuid,
    from: Option<u64>,
    to: Option<u64>,
) -> Result<Pin<Box<dyn EventStream<Self::EventType, Self::Error> + Send + 'life0>>, Self::Error>;
```

Convert `read_events` and `read_events_since` to defaulted methods delegating to it:

```rust
async fn read_events(&self, stream_id: Uuid)
    -> Result<Pin<Box<dyn EventStream<Self::EventType, Self::Error> + Send + 'life0>>, Self::Error>
{
    self.read_events_range(stream_id, None, None).await
}

async fn read_events_since(&self, stream_id: Uuid, version: u64)
    -> Result<Pin<Box<dyn EventStream<Self::EventType, Self::Error> + Send + 'life0>>, Self::Error>
{
    self.read_events_range(stream_id, Some(version), None).await
}
```

**Bound semantics (the contract both backends must reproduce):**

- `read_events_range(id, None, None)` ≡ old `read_events`.
- `read_events_range(id, Some(v), None)` ≡ old `read_events_since(id, v)`.
- `read_events_range(id, None, Some(to))` → only `stream_version <= to`.
- `read_events_range(id, Some(from), Some(to))` → only `from <= stream_version <= to`.
- An empty range (`from > to`) yields zero events (no error).

### 4.2 `epoch_pg` implementation

Replace the `read_events` and `read_events_since` overrides with a single
`read_events_range`. Build the WHERE clause from the optional bounds, omitting each
predicate when `None`, and bind parameters in matching order so the SQL stays sargable on
the existing `(stream_id, stream_version)` index:

- Base: `WHERE stream_id = $1`.
- If `from` is `Some`: append `AND stream_version >= $N` and bind `from as i64`.
- If `to` is `Some`: append `AND stream_version <= $M` and bind `to as i64`.
- Always `ORDER BY stream_version ASC`.

Parameter numbering must be computed dynamically as clauses are added (e.g. start at `$2`,
increment per included bound) so omitting `from` does not leave a hole. Reuse the existing
`try_stream!` + `pg_db_event_to_event` streaming body and `PgEventStream` wrapper unchanged
(the dead-letter `Ok(None)` skip and upcasting path are unaffected). No raw column
functions are applied to `stream_version`, so the index predicate remains usable.

Equivalence: old `read_events` (`>= 0`) → `None` lower bound (no clause) returns the same
rows; old `read_events_since(v)` → `>= v` is unchanged.

### 4.3 `epoch_mem` implementation

`InMemoryEventStoreStream` gains a `to_version: Option<u64>` field
(`epoch_mem/src/event_store.rs:320`). `new` (`:337`) takes the upper bound and stores it.
`poll_next` (`:356`), after locating the candidate event, stops the stream
(`Poll::Ready(None)`) when `event.stream_version > to_version` (i.e. `to_version` is `Some`
and exceeded) — early termination, no over-read.

`read_events_range` maps bounds to the existing index model:

- Lower bound → `start_version` (the `current_index`):
  `from = None` → `0` (equivalent to the old `read_events_since(1)` → index `0`);
  `from = Some(v)` → `v - 1` (equivalent to the old `read_events_since(v)`).
  **The `None` case must branch and not compute `None - 1`** — the existing `version - 1`
  at `:83` would underflow/panic for a `0`/absent bound, so the new code must guard this.
- Upper bound → `to_version` passed straight through to the stream.

Delete the `read_events`/`read_events_since` overrides (`:74`, `:83`); they fall through to
the trait defaults, which call this `read_events_range`.

## 5. Requirements

| ID | Requirement |
|----|-------------|
| R-1 | `EventStoreBackend::read_events_range(stream_id, from: Option<u64>, to: Option<u64>)` exists with the return type matching the existing `read_events` signature (incl. `'life0`). |
| R-2 | `read_events` and `read_events_since` are **default** trait methods delegating to `read_events_range(.., None, None)` and `read_events_range(.., Some(version), None)` respectively. |
| R-3 | `PgEventStore` implements only `read_events_range`; its `read_events`/`read_events_since` overrides are deleted. |
| R-4 | `PgEventStore::read_events_range` builds optional `>= from` / `<= to` predicates with correct dynamic `$N` binding, `ORDER BY stream_version ASC`, reusing `pg_db_event_to_event`; query stays sargable on `UNIQUE (stream_id, stream_version)`. |
| R-5 | `InMemoryEventStore` implements only `read_events_range`; its `read_events`/`read_events_since` overrides are deleted. |
| R-6 | `InMemoryEventStoreStream` gains `to_version: Option<u64>`; `poll_next` terminates when `stream_version > to_version`; `from = None` maps to start index `0` without underflow. |
| R-7 | Behavioural equivalence: `(None, None)` ≡ old `read_events`; `(Some(v), None)` ≡ old `read_events_since(v)`, for both backends. |
| R-8 | `(None, Some(to))` yields only `stream_version <= to`; `(Some(from), Some(to))` yields only `[from, to]`; `from > to` yields empty, for both backends. |
| R-9 | The existing test suite passes **unmodified**. |
| R-10 | New tests cover the `to` bound — closed `[from, to]` and half-open `(None, Some(to))` — for both `epoch_mem` and `epoch_pg`. |
| R-11 | `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` are clean. |

## 6. Test Plan

- **`epoch_mem` (unit, in `event_store.rs` `#[cfg(test)]`):** mirror
  `in_memory_event_store_read_events_since` style with three stored events:
  - `read_events_range(id, None, Some(2))` → `[event1, event2]`.
  - `read_events_range(id, Some(2), Some(3))` → `[event2, event3]`.
  - `read_events_range(id, Some(2), Some(2))` → `[event2]`.
  - `read_events_range(id, Some(3), Some(2))` (empty) → none.
  - Regression assertions that `read_events`/`read_events_since` still match
    (the unmodified existing tests already cover `(None,None)` / `(Some(v),None)`).
- **`epoch_pg` (DB-gated integration, `epoch_pg/tests/pgeventstore_integration_tests.rs`):**
  alongside `test_read_events_since` (`:127`), add a `test_read_events_range` storing three
  versions and asserting the closed and half-open `to`-bound cases above.

## 7. Risks & Mitigations

- **`epoch_mem` off-by-one / underflow.** The existing `version - 1` at `:83` panics on a
  `0`/absent lower bound. *Mitigation:* `from = None` branches to start index `0`; tests
  cover `(None, _)` and `from > to`.
- **`epoch_pg` parameter-index drift.** Omitting the `from` clause must not leave a `$2`
  hole. *Mitigation:* compute `$N` while assembling clauses and bind in the same order;
  integration test exercises both `from`-present and `from`-absent paths.
- **Hidden callers relying on overrides.** `read_events`/`read_events_since` keep identical
  signatures and semantics via defaults; behaviour is unchanged, so callers compile and
  behave as before (R-7, R-9).
- **Trait change is technically breaking for external `EventStoreBackend` implementors**
  (a new required method). *Mitigation:* pre-1.0; documented in §9. In-tree backends both
  implement it.

## 8. Considered Alternatives

- **Shared `epoch_core::testing::verify_read_events_range` parity helper** (mirroring
  `verify_store_events_atomicity` / `verify_upcasting_chain`). Rejected for this ticket to
  avoid scope widening; the ticket calls for per-backend tests. Can be added later if a
  third backend appears.
- **Defaulting `read_events_range` itself** (so existing backends need no change). Rejected:
  it would force the over-read the ticket exists to eliminate; the primitive must be
  backend-native.

## 9. Breaking Change & Migration Notes

`feat(core)!: add read_events_range bounded-replay primitive to EventStoreBackend`

- `EventStoreBackend` gains a **new required method** `read_events_range`. External
  implementors must add it (the two in-tree backends are updated here).
- `read_events` / `read_events_since` keep identical signatures and observable behaviour;
  no caller changes required.
- No schema migration, no new dependency, no event-format change.

---

## Phased Delivery Plan

1. **Phase 1 — `epoch_core` trait** (R-1, R-2): add `read_events_range`; convert
   `read_events`/`read_events_since` to defaults. Workspace will not compile until backends
   implement the new method (Phases 2–3 follow immediately).
2. **Phase 2 — `epoch_mem`** (R-5, R-6, R-7): add `to_version` to
   `InMemoryEventStoreStream`, implement `read_events_range`, delete the two overrides;
   guard the `None` lower bound.
3. **Phase 3 — `epoch_pg`** (R-3, R-4, R-7): implement `read_events_range` with dynamic
   bound predicates, delete the two overrides.
4. **Phase 4 — Tests** (R-8, R-10): add `to`-bound tests (closed + half-open) for both
   backends; confirm the unmodified existing suite still passes (R-9).
5. **Phase 5 — Hygiene & docs** (R-9, R-11): `cargo fmt`, `cargo clippy --all-targets -- -D
   warnings`, full `cargo test`; CHANGELOG breaking-change/migration note.

---

## Phases (JSON)

```json
{
  "phases": [
    { "phase": 1, "focus": "epoch_core: add read_events_range to EventStoreBackend; make read_events and read_events_since default methods delegating to it", "effort": "S", "difficulty": "standard", "requirements": ["R-1","R-2"] },
    { "phase": 2, "focus": "epoch_mem: add to_version field to InMemoryEventStoreStream with poll_next early-termination, implement read_events_range (None lower bound -> index 0, no underflow), delete read_events/read_events_since overrides", "effort": "S", "difficulty": "standard", "requirements": ["R-5","R-6","R-7"] },
    { "phase": 3, "focus": "epoch_pg: implement read_events_range with dynamic optional >= from / <= to predicates and correct $N binding, sargable on UNIQUE(stream_id, stream_version), reuse pg_db_event_to_event; delete read_events/read_events_since overrides", "effort": "S", "difficulty": "standard", "requirements": ["R-3","R-4","R-7"] },
    { "phase": 4, "focus": "Tests: closed and half-open to-bound tests for epoch_mem (unit) and epoch_pg (DB-gated integration), plus empty-range case; verify existing suite passes unmodified", "effort": "S", "difficulty": "standard", "requirements": ["R-8","R-9","R-10"] },
    { "phase": 5, "focus": "Hygiene: cargo fmt, clippy --all-targets -D warnings, full cargo test; CHANGELOG breaking-change + migration note", "effort": "S", "difficulty": "standard", "requirements": ["R-9","R-11"] }
  ]
}
```
