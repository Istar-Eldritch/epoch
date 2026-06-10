# Specification: `read_last_event()` and `Command::with_causation_id()` (CLOUD-168)

**Document ID:** 0015
**Status:** Approved
**Created:** 2026-06-10
**Author:** AI Agent
**Supersedes / Consolidates:** specs 0013 (`0013-event-metadata-query-helpers.md`) and 0014 (`0014-read-last-event-and-causation-builder.md`)
**Tracking Issue:** Linear CLOUD-168 — "epoch: implement read_last_event + Command::with_causation_id (specs 0013/0014)"

---

## 1. Problem Statement

Catacloud background tasks (e.g., stale-machine checkers, periodic billing tasks — 18 call
sites across 8 files) need an entity's **last event metadata** (`id`, `correlation_id`) so
that recovery commands inherit the original correlation chain and appear in the same causal
timeline as the user action that started it.

The current epoch API cannot express this efficiently:

1. `EventStoreBackend::read_events(stream_id)` returns a stream — the caller must consume
   all N events of the stream just to obtain the last one.
2. There is no `read_last_event(stream_id)` method on `EventStoreBackend`.
3. `Command` only offers `.caused_by(&event)` (requires a full `Event` reference) and
   `.with_correlation_id(uuid)`. There is no `.with_causation_id(uuid)` for cases where
   only the causing event's **ID** is available.

Catacloud therefore works around the framework with raw, Postgres-specific SQL against the
`epoch_events` table:

```rust
let corr = sqlx::query_as::<_, EntityCorrelation>(
    "SELECT id as last_event_id, correlation_id FROM epoch_events
     WHERE stream_id = $1 ORDER BY global_sequence DESC LIMIT 1"
)
.bind(entity_id)
.fetch_optional(&postgres)
.await?;
```

This bypasses the `EventStoreBackend` abstraction, couples application code to the epoch
table schema, and is untestable against `InMemoryEventStore`.

## 2. Functional Requirements

Each requirement is independently verifiable.

**FR-1** — `EventStoreBackend` (in `epoch_core/src/event_store.rs`) SHALL expose a new
method `read_last_event(stream_id: Uuid)` returning
`Result<Option<Event<Self::EventType>>, Self::Error>`.

**FR-2** — `read_last_event()` SHALL return `Ok(None)` when the stream does not exist or
contains no events. It SHALL NOT return an error for an empty/unknown stream.

**FR-3** — `read_last_event()` SHALL return the event with the **highest `stream_version`**
of the given stream, with all metadata fields populated as they would be by
`read_events()` (including `id`, `created_at`, `causation_id`, `correlation_id`, and
`global_sequence` when assigned by the backend).

**FR-4** — The trait SHALL provide a **default implementation** of `read_last_event()`
that consumes the `read_events(stream_id)` stream and returns the final element. This
preserves source compatibility for third-party `EventStoreBackend` implementations
(precedent: the `store_events()` default implementation).

**FR-5** — `PgEventStore` (in `epoch_pg/src/event_store.rs`) SHALL override
`read_last_event()` with a single-row query:

```sql
SELECT id, stream_id, stream_version, event_type, data, created_at,
       actor_id, purger_id, purged_at, global_sequence, causation_id, correlation_id
FROM {events_table}
WHERE stream_id = $1
ORDER BY stream_version DESC
LIMIT 1
```

executed with `fetch_optional`, reusing the existing `PgDBEvent` → `Event` builder
conversion used by `read_events_since()` / `read_events_by_correlation_id()`.

> Ordering note: the Linear issue specifies `ORDER BY stream_version DESC`; spec 0013
> specified `ORDER BY global_sequence DESC`. Within a single stream the two orders are
> equivalent (the `UNIQUE (stream_id, stream_version)` constraint forces monotonically
> increasing versions per stream), but `stream_version DESC` is index-backed by that same
> unique index (migration m001), whereas `global_sequence` only has a single-column index
> (m002) not composed with `stream_id`. **Decision: use `stream_version DESC`.** See
> Open Question OQ-1.

**FR-6** — `InMemoryEventStore` (in `epoch_mem/src/event_store.rs`) SHALL override
`read_last_event()` by resolving the last entry of `stream_events[stream_id]` against the
`events` map and cloning out of the `Arc`:

- last event = `data.stream_events.get(&stream_id).and_then(|ids| ids.last())` →
  `data.events.get(id)` → `(**arc).clone()`
- missing stream or empty `Vec` → `Ok(None)`

**FR-7** — `Command<D, C>` (in `epoch_core/src/aggregate.rs`) SHALL gain a builder method:

```rust
/// Explicitly sets a causation ID from a known event ID.
///
/// Prefer [`caused_by`](Self::caused_by) when a full `Event` reference is
/// available — it threads both `causation_id` and `correlation_id`. Use
/// `with_causation_id` when only the causing event's ID is known (e.g., from
/// `EventStoreBackend::read_last_event()` metadata in background/recovery tasks).
pub fn with_causation_id(mut self, causation_id: Uuid) -> Self {
    self.causation_id = Some(causation_id);
    self
}
```

placed alongside the existing `caused_by()` / `with_correlation_id()` methods, with the
same `impl` bounds. It SHALL NOT modify `correlation_id`.

**FR-8** — `with_causation_id()` SHALL be chainable with `with_correlation_id()` in either
order, and a later `caused_by()`/`with_causation_id()` call SHALL overwrite a previously
set `causation_id` (last-write-wins, consistent with existing builder semantics).

**FR-9** — Rustdoc SHALL be added for both new APIs, including a usage example mirroring
the recovery-task scenario:

```rust
if let Some(last_event) = event_store.read_last_event(machine_id).await? {
    let cmd = Command::new(pool_id, ForceReleaseMachine { /* .. */ }, creds, None)
        .caused_by(&last_event);
    aggregate.handle(cmd).await?;
}
```

## 3. Non-Functional Requirements

**NFR-1 (Performance)** — `PgEventStore::read_last_event()` MUST issue exactly one SQL
query returning at most one row, using the `UNIQUE (stream_id, stream_version)` index
(no sequential scan, no full-stream fetch). `InMemoryEventStore::read_last_event()` MUST
be O(1) map/vector lookups plus one event clone.

**NFR-2 (Backward compatibility)** — No existing public API signature changes. The new
trait method has a default implementation, so external `EventStoreBackend` implementors
keep compiling. `with_causation_id()` is purely additive.

**NFR-3 (Code quality)** — `cargo fmt` clean; `cargo clippy -- -D warnings` clean; no
`unwrap()`/`expect()` outside tests (reuse the existing error-mapping patterns in
`epoch_pg`).

**NFR-4 (Documentation)** — All new public items carry rustdoc (project convention §5.0).
`CHANGELOG.md` `[Unreleased] / Added` gains entries for both APIs.

**NFR-5 (Testing)** — New tests are additive, independent, idempotent, and follow the
existing harnesses (`#[serial]` + `Migrator` for `epoch_pg`, inline `mod tests` for
`epoch_mem`, integration test file for `epoch_core` command builders).

## 4. Success Criteria

The change is complete when all of the following hold:

- **SC-1:** `cargo test` passes across the workspace, including new tests listed in §6.5.
- **SC-2:** `cargo clippy --workspace -- -D warnings` and `cargo fmt --check` pass.
- **SC-3:** `PgEventStore` and `InMemoryEventStore` both have explicit, tested
  `read_last_event()` overrides satisfying FR-2/FR-3/FR-5/FR-6.
- **SC-4:** A third-party-style backend (test double implementing only the previously
  required methods) still compiles and gets a working `read_last_event()` via the default
  implementation (FR-4) — verified by a unit test in `epoch_core`.
- **SC-5:** `Command::with_causation_id()` exists with tests covering FR-7/FR-8.
- **SC-6:** The `event-correlation-causation` example demonstrates
  `read_last_event()` + causation threading and runs successfully
  (`cargo run --example event-correlation-causation`).
- **SC-7 (downstream, tracked in CLOUD-168, not in this repo):** Catacloud's raw-SQL
  last-event-metadata call sites are migrated to `read_last_event()`; no raw queries
  against `epoch_events` remain for this purpose.

## 5. Scope and Boundaries

### In scope (this repository)

1. `EventStoreBackend::read_last_event()` trait method with default implementation.
2. `PgEventStore` and `InMemoryEventStore` overrides.
3. `Command::with_causation_id()` builder.
4. Tests (unit + integration) for all of the above.
5. Update of `epoch/examples/event-correlation-causation.rs`.
6. `CHANGELOG.md` entries.

### Out of scope

1. **`read_last_event_metadata()` / `EventMetadata` struct** (optional item in specs
   0013/0014). Deferred: `read_last_event()` is sufficient for current catacloud needs;
   the JSONB deserialization cost of one event is negligible. Revisit if profiling shows
   otherwise (see OQ-2).
2. **Catacloud call-site migration** — lives in the catacloud repository; tracked under
   CLOUD-168 acceptance but not implementable here.
3. New Postgres migrations — no schema change is required; the existing
   `UNIQUE (stream_id, stream_version)` index serves the query.
4. Changes to `read_events*`, `store_event*`, correlation/causation tracing APIs.
5. Crate version bump beyond the CHANGELOG entry — workspace is pre-1.0 (`0.1.0`,
   unreleased changes already pending); release versioning is handled at release time.

## 6. Solution Approach

### 6.1 Files to modify

| File | Change |
|---|---|
| `epoch_core/src/event_store.rs` | Add `read_last_event()` to `EventStoreBackend` with default impl |
| `epoch_core/src/aggregate.rs` | Add `Command::with_causation_id()` |
| `epoch_pg/src/event_store.rs` | Override `read_last_event()` for `PgEventStore` |
| `epoch_mem/src/event_store.rs` | Override `read_last_event()` for `InMemoryEventStore`; inline tests |
| `epoch_core/tests/command_causation_tests.rs` | Tests for `with_causation_id()` |
| `epoch_core/tests/` (new file, e.g. `read_last_event_default_impl_tests.rs`) | Default-impl test with a minimal backend double |
| `epoch_pg/tests/pgeventstore_integration_tests.rs` | Integration tests for Pg override |
| `epoch/examples/event-correlation-causation.rs` | Demonstrate `read_last_event()` + `with_causation_id()` |
| `CHANGELOG.md` | `[Unreleased] / Added` entries |

### 6.2 Trait change (`epoch_core/src/event_store.rs`)

Append to `EventStoreBackend` (after `store_events`, before
`read_events_by_correlation_id`):

```rust
/// Returns the most recent event of the given stream, or `None` if the
/// stream is empty or does not exist.
///
/// "Most recent" means the event with the highest `stream_version`.
///
/// This is the framework-supported way for background tasks to obtain the
/// last event's metadata (`id`, `correlation_id`) in order to thread
/// correlation/causation context into recovery commands — without consuming
/// the full event stream or querying the storage backend directly.
///
/// # Default implementation
///
/// The default implementation consumes the [`read_events`](Self::read_events)
/// stream and returns its final element (O(N) in stream length). Backends
/// should override this with an efficient single-row lookup.
async fn read_last_event(
    &self,
    stream_id: Uuid,
) -> Result<Option<Event<Self::EventType>>, Self::Error> {
    let mut stream = self.read_events(stream_id).await?;
    let mut last = None;
    while let Some(event) = stream.next().await {
        last = Some(event?);
    }
    Ok(last)
}
```

Implementation note: the default body requires `futures_util::StreamExt` (or a manual
`poll_next` loop using the already-imported `futures_core::Stream`). `epoch_core` does not
currently depend on `futures_util`; prefer a small manual poll loop or add `futures-util`
as a dependency — decide at implementation time for minimal dependency footprint
(CLAUDE.md §5.0). If `futures-util` is added, justify it in the commit message.

### 6.3 `PgEventStore` override (`epoch_pg/src/event_store.rs`)

```rust
async fn read_last_event(
    &self,
    stream_id: Uuid,
) -> Result<Option<Event<Self::EventType>>, Self::Error> {
    let last_sql = format!(
        "SELECT id, stream_id, stream_version, event_type, data, created_at, \
         actor_id, purger_id, purged_at, global_sequence, causation_id, correlation_id \
         FROM {} WHERE stream_id = $1 ORDER BY stream_version DESC LIMIT 1",
        self.events_table,
    );
    let row: Option<PgDBEvent> = sqlx::query_as(&last_sql)
        .bind(stream_id)
        .fetch_optional(&self.postgres)
        .await?;

    let Some(entry) = row else { return Ok(None) };
    // PgDBEvent -> Event conversion: identical builder sequence to
    // read_events_since()/read_events_by_correlation_id(). Extract the
    // existing duplicated conversion into a private helper
    // `fn db_event_to_event(entry: PgDBEvent) -> Result<Event<B::EventType>, Self::Error>`
    // and reuse it from all three call sites.
    Ok(Some(db_event_to_event(entry)?))
}
```

The conversion helper extraction is an internal refactor (`refactor(pg)` or folded into
the feature commit); it removes the third copy of the builder block rather than adding it.
Error mapping follows the existing `PgEventStoreError::{DBError, DeserializeEventError,
BuildEventError}` variants. Respect `with_table()` custom table names via
`self.events_table` (as shown).

### 6.4 `InMemoryEventStore` override (`epoch_mem/src/event_store.rs`)

```rust
async fn read_last_event(
    &self,
    stream_id: Uuid,
) -> Result<Option<Event<Self::EventType>>, Self::Error> {
    let data = self.data.lock().await;
    Ok(data
        .stream_events
        .get(&stream_id)
        .and_then(|ids| ids.last())
        .and_then(|id| data.events.get(id))
        .map(|arc| (**arc).clone()))
}
```

(`stream_events` vectors are append-ordered by `store_event`, so `.last()` is the
highest-version event.)

### 6.5 Tests

**`epoch_core/tests/command_causation_tests.rs` (extend):**
1. `with_causation_id_sets_causation_only` — sets `causation_id`, leaves
   `correlation_id` untouched (`None`).
2. `with_causation_id_chains_with_correlation_id` — both builders chained in both
   orders produce both fields.
3. `with_causation_id_overwrites_previous_value` — second call (or call after
   `caused_by`) wins.

**`epoch_core/tests/read_last_event_default_impl_tests.rs` (new):**
4. Minimal backend double implementing only the previously required methods
   (`read_events` backed by `SliceEventStream`, stub others): default
   `read_last_event()` returns the last event of a multi-event stream.
5. Default impl returns `Ok(None)` for an empty stream.

**`epoch_mem/src/event_store.rs` inline `mod tests` (extend):**
6. `read_last_event_returns_latest_event` — store 3 events, expect version-3 event with
   matching `id`/`correlation_id`/`causation_id`.
7. `read_last_event_returns_none_for_unknown_stream`.
8. `read_last_event_isolated_per_stream` — two streams; each returns its own last event.

**`epoch_pg/tests/pgeventstore_integration_tests.rs` (extend, `#[serial]`, random
`Uuid::new_v4()` stream IDs for idempotency/parallel safety):**
9. `test_read_last_event_returns_latest` — store events v1..v3 (with
   correlation/causation set), assert returned event equals the stored v3 event including
   `global_sequence: Some(_)`, `correlation_id`, `causation_id`, `created_at`.
10. `test_read_last_event_empty_stream_returns_none` — fresh random stream ID → `Ok(None)`.
11. `test_read_last_event_respects_custom_table` — `PgEventStore::with_table(...)` store +
    read-back (mirrors existing custom-table test patterns if present; otherwise verify via
    default table only and drop this case — see OQ-3).

**Example (`epoch/examples/event-correlation-causation.rs`):**
12. Add a final "recovery task" section: after the saga flow completes, call
    `event_store.read_last_event(order_id)`, build a command with
    `.with_causation_id(last_event.id).with_correlation_id(last_event.correlation_id.unwrap_or(last_event.id))`,
    dispatch it, and show (via the existing timeline printout) that the recovery event
    joins the same correlation tree.

### 6.6 Changelog

Under `[Unreleased] / Added`:

```markdown
- `EventStoreBackend::read_last_event(stream_id)` returning the most recent event of a
  stream (`Option<Event>`), with a default implementation and efficient overrides in
  `PgEventStore` and `InMemoryEventStore`
- `Command::with_causation_id(uuid)` builder for threading causation when only the
  causing event's ID is available
```

### 6.7 Suggested commit sequence (TDD, conventional commits)

1. `test(core): add failing tests for Command::with_causation_id`
2. `feat(command): add Command::with_causation_id builder`
3. `test(event-store): add default read_last_event tests with minimal backend double`
4. `feat(event-store): add EventStoreBackend::read_last_event with default impl`
5. `test(mem): add read_last_event tests` / `feat(mem): implement read_last_event`
6. `test(pg): add read_last_event integration tests` /
   `feat(pg): implement read_last_event with single-row query` (includes the
   `PgDBEvent`→`Event` helper extraction)
7. `docs(event-store): demonstrate read_last_event in correlation example; update changelog`

## 7. Open Questions

**OQ-1 — Ordering column: `stream_version` vs `global_sequence`.** ✅ RESOLVED
Linear CLOUD-168 says `ORDER BY stream_version DESC`; spec 0013 says
`ORDER BY global_sequence DESC`. Per-stream they are semantically equivalent (the
`UNIQUE (stream_id, stream_version)` constraint serializes versions), and
`stream_version DESC` is directly served by that composite unique index while
`global_sequence` has only a single-column index. **Decision: use
`stream_version DESC`** (also makes the trait contract — "highest `stream_version`" —
backend-agnostic, since `InMemoryEventStore` does not assign `global_sequence`).
_Approved by reviewer 2026-06-10._

**OQ-2 — Defer `read_last_event_metadata()` / `EventMetadata`?** ✅ RESOLVED
Both discovery docs mark it optional/low-priority. **Decision: defer (out of scope §5).**
If catacloud later shows measurable JSONB deserialization cost on hot paths, add it in a
follow-up spec.
_Approved by reviewer 2026-06-10._

**OQ-3 — Custom-table test coverage.**
`PgEventStore::with_table()` exists; the SQL must use `self.events_table` (FR-5 already
mandates this). Decide whether a dedicated custom-table integration test (test #11) is
required or whether code review of the `format!` usage suffices, matching whatever the
existing test suite does for `read_events_by_correlation_id`.

**OQ-4 — Purged events.**
The last event of a stream may be purged (`data: None`, `purger_id`/`purged_at` set).
`read_last_event()` returns it as-is (consistent with `read_events()`); callers only need
metadata, which survives purging. Confirm no filtering is desired.

## 8. Risks

| # | Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|---|
| R-1 | Default trait impl is O(N); third-party backends silently inherit slow behavior | Medium | Low | Rustdoc explicitly warns and instructs overriding; both first-party backends override |
| R-2 | Default impl pulls in `futures_util::StreamExt` in `epoch_core`, growing the dependency tree | Low | Low | Prefer a manual `poll_fn`/poll loop using existing `futures_core`; only add `futures-util` if the manual loop is unreasonably awkward (justify per CLAUDE.md §5.0) |
| R-3 | `with_causation_id()` used without correlation threading produces events with `causation_id` but mismatched/auto-generated `correlation_id`, fragmenting timelines | Medium | Medium | Rustdoc steers users to `caused_by()` when a full event is available; example shows pairing with `with_correlation_id()` |
| R-4 | Divergence between this implementation (`stream_version DESC`) and catacloud's existing raw SQL (`global_sequence DESC`) could surface ordering edge cases during migration | Low | Low | Per-stream equivalence argued in OQ-1; Pg integration test asserts the highest-version event is returned after multiple appends |
| R-5 | Refactoring the `PgDBEvent`→`Event` conversion into a shared helper could subtly change behavior of `read_events_since`/`read_events_by_correlation_id` | Low | Medium | Pure mechanical extraction; existing integration tests for both methods act as regression guards |
| R-6 | Concurrent append during `read_last_event()` returns a momentarily stale "last" event | Inherent | Low | Acceptable by design — event-sourced reads are snapshots; recovery commands use optimistic concurrency (`aggregate_version`) downstream |

## 9. Expected Outcome

- Catacloud (and any epoch consumer) can fetch a stream's last event through the
  `EventStoreBackend` abstraction with one cheap query, on both Postgres and in-memory
  backends.
- Commands can be causally linked from bare event IDs via
  `Command::with_causation_id(uuid)`, complementing `caused_by(&event)`.
- No breaking changes for existing implementors; raw-SQL workarounds in catacloud can be
  deleted (CLOUD-168 acceptance, downstream).
