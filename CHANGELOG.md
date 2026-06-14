# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
