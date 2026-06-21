# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **`read_events_range` bounded-replay primitive** (`epoch_core`, `epoch_pg`, `epoch_mem`, CLOUD-183) —
  a new `EventStoreBackend::read_events_range(stream_id, from: Option<u64>, to: Option<u64>)`
  primitive that pushes inclusive `[from, to]` `stream_version` bounds down to storage,
  eliminating the full-stream over-read previously required for upper-bounded replay:
  - **`epoch_core`** — `read_events_range` added as the new required method;
    `read_events` and `read_events_since` converted to default methods delegating to it
    (`None, None` and `Some(version), None` respectively). ⚠ **Breaking change** for external
    `EventStoreBackend` implementors: the new required method must be added.
  - **`epoch_pg`** — `PgEventStore::read_events_range` builds optional `>= from` / `<= to`
    predicates with dynamic `$N` binding, staying sargable on the existing
    `UNIQUE (stream_id, stream_version)` index; the `read_events`/`read_events_since`
    overrides are removed.
  - **`epoch_mem`** — `InMemoryEventStore::read_events_range` with early-termination on
    the upper bound; `InMemoryEventStoreStream` gains `to_version: Option<u64>`; the
    `read_events`/`read_events_since` overrides are removed.
  - No schema migration, no new dependency, no event-format change.

- **Event schema evolution / upcasting mechanism** (`epoch_core`, `epoch_pg`, `epoch_derive`, CLOUD-173) —
  a first-class, versioned, fail-loud-by-default upcasting system so that historic
  persisted events can be transformed forward to the current schema before
  deserialization, eliminating both hard replay aborts and silent event loss:
  - **`epoch_core::upcasting`** (new, opt-in `upcasting` feature) — `Upcaster` trait,
    `UpcasterRegistry`, `FailurePolicy` (`Fail` | `DeadLetter`), `UpcastError`,
    `UpcastContext`, `DeadLetterSink`, `DeadLetteredEvent`, `UpcastCounters`.
    An empty registry (the default) is a zero-cost no-op: events that still deserialize
    cleanly behave exactly as before.
  - **`EventData::schema_version()`** — new defaulted method returning `1`; all
    existing `EventData` impls compile unchanged. `#[derive(EventData)]` accepts an
    optional `#[event_data(schema_version = N)]` enum-level attribute.
  - **`Event<D>` / `EventBuilder<D>`** gain a `schema_version: SchemaVersion` field
    (default `1`), populated from `EventData::schema_version()` on the write path and
    from the stored column on the read path, and preserved through all envelope
    conversions (`to_subset_event`, `to_subset_event_ref`, `to_superset_event`,
    `into_builder`, `build`).
  - **Migration `m012_add_schema_version_to_events`** — adds a nullable
    `schema_version INT DEFAULT 1` column to `epoch_events` via
    `ADD COLUMN IF NOT EXISTS`; no backfill, no table rewrite. Pre-migration rows keep
    `NULL`, interpreted as version `1` by the read path. Registered at version 12.
  - **`PgEventStore::with_upcasters(pool, bus, Arc<UpcasterRegistry>)`** — new
    constructor that wires the registry into the read path; the existing constructors
    set an empty registry (identical behavior for projects that register no upcasters).
  - **`PgEventStoreError::Upcast`** — new variant that wraps `UpcastError` and
    propagates hard failures through the event stream under `FailurePolicy::Fail`.
  - **`ensure_schema_version_column`** helper (`epoch_pg`) — idempotent
    `ADD COLUMN IF NOT EXISTS` for custom tables created via `PgEventStore::with_table`.
  - **`epoch_core::testing::verify_upcasting_chain`** — reusable contract-test helper
    (gated behind the `testing` feature, usable from downstream crates) that asserts a
    multi-step upcasting chain correctly advances a v1 payload to the current version
    and deserializes it.
  - **`docs/schema-evolution.md`** — new design guidance document covering the
    change-kind matrix, worked examples for all five mutation kinds (add optional field,
    rename field, remove field, change field type, add required field with default),
    failure policy trade-offs, the split/merge pattern, and an operational checklist.
  - Upcasting observability counters (`applied`, `succeeded`, `failed`,
    `dead_lettered`) accessible via `UpcasterRegistry::counters()`; DEBUG/ERROR
    structured logs emitted per step and per failure.

- **`EventStoreBackend::store_events()` atomicity contract test** (`epoch_core`, CLOUD-171) —
  a reusable, backend-agnostic helper that verifies any backend's `store_events()` honours
  the all-or-nothing persistence guarantee:
  - `epoch_core::testing::verify_store_events_atomicity(backend, make_event)` seeds one
    event, submits a mid-batch duplicate-version batch (which must be rejected), then
    asserts that *no* event from the failed batch was persisted — catching partial writes.
    It also exercises an empty-batch no-op and a fully-valid batch.
  - Gated behind the new `testing` feature in `epoch_core`; never ships in production
    builds. Enable it in `[dev-dependencies]` to run the helper from your own test suite.
  - Both first-party backends (`epoch_mem`, `epoch_pg`) run this contract test as part
    of their integration test suites and pass with no behavioral change.
- **Snapshot fencing for gap resolution** (`epoch_pg`, CLOUD-180) — the event-bus
  gap resolver now *proves* whether a `global_sequence` gap can still be filled by
  an in-flight transaction instead of guessing on a wall-clock timer:
  - Migration `m011_add_txid_to_events` adds a nullable `txid BIGINT` column to
    `epoch_events` with a `DEFAULT (pg_current_xact_id()::text::bigint)` (PostgreSQL
    **13+** only) and a partial index. Existing rows keep `NULL` (no backfill, no
    table rewrite).
  - The reader queries the current transaction-id snapshot
    (`pg_snapshot_xmin/xmax(pg_current_snapshot())`) **once per batch that has
    active or newly-detected gaps** and *holds* the checkpoint for a gap whose
    writer is still in-flight until it commits (gap fills) or aborts (gap proven
    permanent — advanced as `FenceCleared`, silently, with no record).
  - `gap_timeout` is demoted to a **backstop**: it still fires (and records via the
    CLOUD-169 machinery, plus a dedicated persistent-pin `WARN` with the current
    `xmin`/`fence_xmax`) for pathological cases where the fence cannot clear
    (abandoned prepared transactions, `idle-in-transaction` sessions pinning `xmin`).
  - `ReliableDeliveryConfig::snapshot_fencing` (defaults to `true`) toggles the
    feature; `false` reverts to the legacy timeout-only resolver. Fencing also
    auto-disables for a batch if the snapshot query fails or the `txid` primitives
    are unavailable — degrading gracefully without panics.
  - `PgEventStore::with_table` (and `PgEventBus` init for a custom `events_table`)
    auto-migrates the custom table at startup via an idempotent
    `ADD COLUMN IF NOT EXISTS` + `SET DEFAULT` + `CREATE INDEX IF NOT EXISTS`; on
    failure it logs a `warn!` and degrades to timeout-only for that table.
  - **PostgreSQL 13+ is now the minimum supported version** for the event bus
    (uses the `pg_current_xact_id` / `pg_current_snapshot` epoch-extended id APIs).
- **Gap-timeout observability** (`epoch_pg`) — when a subscriber's checkpoint is
  advanced past a missing `global_sequence` due to `gap_timeout`, the event bus now:
  - Emits a `WARN` log with `bus_name`, `subscriber_id`, `skipped_sequence`, and
    `gap_duration` for immediate operational visibility.
  - Persists a durable record to the new `epoch_event_bus_gap_timeouts` table
    (migration m009, fire-and-forget detached task — does **not** block checkpoint
    advancement).
  - Invokes the optional `on_gap_timeout` callback (see `GapTimeoutCallback` /
    `GapTimeoutInfo`) after a new record is persisted (exactly once per durable
    record), enabling metrics counters, alerting, and custom recovery actions.
- `GapTimeoutCallback` trait and `GapTimeoutInfo` struct (`epoch_pg`) — the
  callback counterpart to `DlqCallback`; set via
  `ReliableDeliveryConfig::on_gap_timeout`.
- `GapTimeoutEntry` struct (`epoch_pg`) — represents one row in
  `epoch_event_bus_gap_timeouts`, returned by `list_gap_timeouts`.
- `PgEventBus::list_gap_timeouts(subscriber_id, unresolved_only, offset, limit)` —
  paginated query for gap-timeout records scoped to this bus; supports optional
  per-subscriber filter and `unresolved_only` flag.
- `PgEventBus::resolve_gap_timeout(id, resolved_by, resolution_notes)` — marks a
  gap-timeout record as resolved; returns `true` on success, `false` if already
  resolved or not found (idempotent second call).
- Migration `m009_create_gap_timeout_log` — creates the
  `epoch_event_bus_gap_timeouts` table with a `UNIQUE (bus_name, subscriber_id,
  skipped_sequence)` constraint (making the recording insert idempotent), a
  sequence index, and a partial unresolved index.
- `GapTimeoutCallback`, `GapTimeoutInfo`, `GapTimeoutEntry` are re-exported from
  `epoch_pg` (and transitively from `epoch`).
- Integration tests can require a live database with `EPOCH_REQUIRE_DB=1`
  (`epoch_pg`) — turns the graceful "skip when Postgres is unreachable" behaviour
  into a hard failure, for CI environments where skipping would mask
  misconfiguration.
- `EventStoreBackend::read_last_event(stream_id)` returning the most recent event of a
  stream (`Option<Event>`), with a default implementation and efficient overrides in
  `PgEventStore` and `InMemoryEventStore`
- `Command::with_causation_id(uuid)` builder for threading causation when only the
  causing event's ID is available
- `CheckpointMode::Batched` for high-throughput scenarios with configurable batch size and max delay
- `CheckpointMode::batched()` and `CheckpointMode::batched_default()` helper constructors
- `InstanceMode::Coordinated` for multi-instance coordination using PostgreSQL advisory locks
- `ProjectionHandler<P>` wrapper type for subscribing projections to the event bus
- `SagaHandler<S>` wrapper type for subscribing sagas to the event bus
- `Event::to_subset_event_ref()` method for reference-based event conversion
- `TryFrom<&D>` implementation generated by `#[subset_enum]` macro for efficient reference-based conversion
- `RefEventStream` trait for reference-based event streaming (internal use)
- `SliceRefEventStream` for zero-copy event iteration from slices
- `Projection::re_hydrate_from_refs()` method for reference-based event stream processing
- Improved documentation for event ownership model

### Changed

- **BREAKING**: `EventStoreBackend::store_events()` no longer has a default implementation
  (`epoch_core`, CLOUD-171). It is now a required trait method. Third-party backends that
  previously relied on the (non-atomic) default loop must provide an explicit implementation.
  See the migration guide below.
- **BREAKING**: `EventBus::publish` now takes `Arc<Event<T>>` instead of `Event<T>`
- **BREAKING**: `EventObserver::on_event` now takes `Arc<Event<ED>>` instead of `Event<ED>`
- **BREAKING**: `Projection::apply_and_store` now takes `&Event<ED>` instead of `Event<ED>`
- **BREAKING**: `Saga::handle_event` now takes `&Event<Self::EventType>` instead of `Event<Self::EventType>`
- **BREAKING**: `Saga::process_event` now takes `&Event<ED>` instead of `Event<ED>`
- **BREAKING**: Projections must now be wrapped in `ProjectionHandler` to subscribe to the event bus
- **BREAKING**: Sagas must now be wrapped in `SagaHandler` to subscribe to the event bus
- **BREAKING**: `Projection::EventType` now requires `TryFrom<&ED, Error = EnumConversionError>` instead of `TryFrom<ED>`
- **BREAKING**: `Saga::EventType` now requires `TryFrom<&ED, Error = EnumConversionError>` instead of `TryFrom<ED>`
- `InMemoryEventStore` now stores events as `Arc<Event<D>>` internally for efficient sharing
- Internal event conversion now uses `to_subset_event_ref()` for better performance
- **Source-compat note**: `ReliableDeliveryConfig` (`epoch_pg`) gains the new
  `on_gap_timeout: Option<Arc<dyn GapTimeoutCallback>>` field (defaults to `None`).
  Code that constructs `ReliableDeliveryConfig` using struct-literal syntax (rather than
  `..Default::default()`) must add `on_gap_timeout: None` to the literal.
- **Source-compat note**: `ReliableDeliveryConfig` (`epoch_pg`) gains the new
  `snapshot_fencing: bool` field (defaults to `true`). Code that constructs
  `ReliableDeliveryConfig` using struct-literal syntax (rather than
  `..Default::default()`) must add `snapshot_fencing: true` (or `false`) to the literal.
- **Source-compat note** (`epoch_core`, CLOUD-173): `Event<D>` and `EventBuilder<D>`
  gain a new `schema_version: SchemaVersion` field (default `1`). Code that constructs
  `Event` or `EventBuilder` via struct-literal syntax (rather than the builder) must
  add `schema_version: 1`. Builder users (`Event::builder()…build()`) are unaffected.
- **Source-compat note** (`epoch_pg`, CLOUD-173): `PgEventStoreError` gains a new
  `Upcast(epoch_core::upcasting::UpcastError)` variant. Match arms that previously
  exhaustively matched `PgEventStoreError` must add `Upcast(_)` (or use a wildcard arm).

### Removed

- Blanket `EventObserver` implementation for `Projection` (replaced with `ProjectionHandler`)
- `EventObserver` supertrait requirement from `Saga` trait

### Performance

- Eliminated unnecessary event cloning when publishing to multiple subscribers
- Events are now shared via `Arc` instead of cloned for each observer
- Saga and projection event handlers receive references, avoiding clones
- `#[subset_enum]` macro generates `TryFrom<&D>` which only clones matched variant's fields instead of the entire enum
- `Aggregate::handle` now uses `SliceRefEventStream` to avoid cloning events during internal re-hydration

### Migration Guide

#### Implementing `EventStoreBackend::store_events()` (CLOUD-171)

`store_events()` is now a **required** trait method. Any third-party backend that previously
relied on the removed non-atomic default loop must add an explicit implementation.

**If your backend supports transactions** (e.g. SQL), wrap all inserts in a single
transaction and publish only after committing:

```rust
async fn store_events(
    &self,
    events: Vec<Event<Self::EventType>>,
) -> Result<(), Self::Error> {
    if events.is_empty() {
        return Ok(());
    }
    // Begin transaction, insert all events, commit, then publish.
    // See `PgEventStore::store_events` for a complete example.
    todo!()
}
```

**If your backend is in-memory or lock-based**, validate all `stream_version`s first
(under the lock), then commit all events, then publish after releasing the lock:

```rust
async fn store_events(
    &self,
    events: Vec<Event<Self::EventType>>,
) -> Result<(), Self::Error> {
    if events.is_empty() {
        return Ok(());
    }
    // Hold lock: validate all versions, then write all events.
    // See `InMemoryEventStore::store_events` for a complete example.
    todo!()
}
```

**Verify your implementation** using the portable contract test (add the `testing`
feature to your `[dev-dependencies]`):

```toml
[dev-dependencies]
epoch_core = { version = "0.1", features = ["testing"] }
```

```rust
#[tokio::test]
async fn my_backend_satisfies_atomicity_contract() {
    let backend = MyBackend::new();
    epoch_core::testing::verify_store_events_atomicity(backend, |stream_id, version| {
        Event::<MyEventType>::builder()
            .stream_id(stream_id)
            .stream_version(version)
            // ... build event ...
            .build()
            .unwrap()
    })
    .await;
}
```

#### Subscribing Projections

```rust
// Before
event_bus.subscribe(my_projection).await?;

// After
use epoch::ProjectionHandler;
event_bus.subscribe(ProjectionHandler::new(my_projection)).await?;
```

#### Subscribing Sagas

```rust
// Before (with manual EventObserver impl)
event_bus.subscribe(my_saga).await?;

// After
use epoch::SagaHandler;
event_bus.subscribe(SagaHandler::new(my_saga)).await?;
```

#### Implementing Saga::handle_event

```rust
// Before
async fn handle_event(
    &self,
    state: Self::State,
    event: Event<Self::EventType>,
) -> Result<Option<Self::State>, Self::SagaError> {
    match event.data {
        // ...
    }
}

// After
async fn handle_event(
    &self,
    state: Self::State,
    event: &Event<Self::EventType>,  // Now takes reference
) -> Result<Option<Self::State>, Self::SagaError> {
    match &event.data {  // Borrow the data
        // ...
    }
}
```

#### Custom EventObserver Implementations

```rust
// Before
async fn on_event(&self, event: Event<ED>) -> Result<(), ...> {
    // use event
}

// After
async fn on_event(&self, event: Arc<Event<ED>>) -> Result<(), ...> {
    // use &*event or event.field (auto-deref)
}
```

#### EventType Trait Bound Change

`Projection::EventType` and `Saga::EventType` now require `TryFrom<&ED, Error = EnumConversionError>`
instead of `TryFrom<ED>`. This enables efficient reference-based conversion internally.

For subset enums generated by `#[subset_enum]`, this is automatic - the macro generates both impls.

For identity conversions (where `EventType == ED`), add this impl:

```rust
impl TryFrom<&MyEvent> for MyEvent {
    type Error = EnumConversionError;
    
    fn try_from(value: &MyEvent) -> Result<Self, Self::Error> {
        Ok(value.clone())
    }
}
```

#### Using Reference-Based Event Conversion

The `#[subset_enum]` macro now generates `TryFrom<&D>` in addition to `TryFrom<D>`.
The framework internally uses `to_subset_event_ref()` for efficient conversion that
only clones matched variant fields instead of the entire enum.

You can also use it directly:

```rust
// Reference-based conversion (only clones matched variant's fields)
let subset_event = event.to_subset_event_ref::<SubsetEvent>()?;
```

#### Event Schema Evolution / Upcasting (CLOUD-173)

This release adds an opt-in upcasting mechanism for evolving persisted event schemas.
For projects that do **not** register any upcasters, behavior is unchanged — the only
visible change is a new nullable `schema_version` column in the database.

**Step 1 — Run pending migrations** (adds the `schema_version` column; idempotent):

```rust
epoch_pg::Migrator::run(&pool).await?;
```

No backfill required. Pre-migration rows have `schema_version = NULL`, which the
read path interprets as version `1`.

**Step 2 — (only if you have breaking event-type changes)** Write an upcaster, bump
the `schema_version` on the type, and wire the registry:

```rust
use epoch_core::upcasting::{Upcaster, UpcastContext, UpcastError, SchemaVersion,
                            UpcasterRegistry};
use epoch_pg::PgEventStore;
use serde_json::Value;
use std::sync::Arc;

// 1. Define the upcaster for OrderPlaced v1 → v2 (e.g., rename a field).
struct OrderPlacedV1ToV2;
impl Upcaster for OrderPlacedV1ToV2 {
    fn event_type(&self) -> &str { "OrderPlaced" }
    fn from_version(&self) -> SchemaVersion { 1 }
    fn upcast(&self, _ctx: &UpcastContext<'_>, mut payload: Value)
        -> Result<Value, UpcastError>
    {
        if let Value::Object(map) = &mut payload {
            if let Some(v) = map.remove("amount") {
                map.insert("total_amount".to_string(), v);
            }
        }
        Ok(payload)
    }
}

// 2. Build the registry.
let mut registry = UpcasterRegistry::new();
registry.register(OrderPlacedV1ToV2);

// 3. Wire it to the store.
let store = PgEventStore::with_upcasters(pool, bus, Arc::new(registry));
```

**Step 3 — Bump the schema version on your event type:**

```rust
#[derive(Debug, Clone, Serialize, Deserialize, EventData)]
#[event_data(schema_version = 2)]   // ← bump here
pub enum AppEvent {
    OrderPlaced { total_amount: u64 },
}
```

For the full change-kind matrix, worked examples, failure-policy guidance, and an
operational checklist, see [docs/schema-evolution.md](docs/schema-evolution.md).
