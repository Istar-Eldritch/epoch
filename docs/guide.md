# Epoch Guide

Epoch is a Rust framework for event-sourced systems using CQRS patterns.

## Core Idea

**Aggregates are living snapshots.** State is persisted immediately after every command — no event replay on reads. Events are still stored for audit, projections, and history.

This is the default path. An opt-in **versioned snapshot store** layers on top for use cases that need historical state at a specific past version — audits, time-travel queries, debugging — without changing the write path or normal read performance.

```
                   ┌──────────┐
          ┌────────│ Command  │
          │        └──────────┘
          ▼
    ┌───────────┐
    │ Aggregate │
    └─────┬─────┘
          │ events + state persisted atomically
          │
    ┌─────┼──────────────┐
    ▼     ▼              ▼
┌───────┐ ┌───────┐ ┌──────────┐
│ Event │ │ State │ │ Event    │
│ Store │ │ Store │ │ Bus      │
└───────┘ └───────┘ └────┬─────┘
                         │
               ┌─────────┼─────────┐
               ▼                   ▼
        ┌─────────────┐    ┌─────────────┐
        │ Projections │    │   Sagas     │
        │ (read model)│    │ (orchestrate│
        └─────────────┘    │  processes) │
                           └──────┬──────┘
                                  │ dispatch commands
                                  └──────────────┘
```

## Commands

Commands represent intentions to change state. They carry an aggregate ID, data, optional credentials, and optional version for optimistic concurrency:

```rust
let cmd = Command::new(user_id, CreateUser { name }, Some(credentials), None);
aggregate.handle(cmd).await?;
```

**Optimistic concurrency** — pass an expected version to prevent conflicting updates:

```rust
let cmd = Command::new(user_id, UpdateName { name }, None, Some(expected_version));
// Returns HandleCommandError::VersionMismatch if state has changed
```

**Subset commands** — like events, commands use `#[subset_enum]` so each aggregate only handles its own commands:

```rust
#[subset_enum(UserCommand, CreateUser, UpdateName)]
#[subset_enum(OrderCommand, PlaceOrder, CancelOrder)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AppCommand { /* ... */ }
```

## Events

Immutable facts. The source of truth. Defined as enums with `#[derive(EventData)]`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, EventData)]
pub enum AppEvent {
    UserCreated { name: String },
    EmailChanged { new_email: String },
    OrderPlaced { product: String },
}
```

Use `#[subset_enum]` to create focused event types for projections and sagas:

```rust
#[subset_enum(UserEvent, UserCreated, EmailChanged)]
#[subset_enum(OrderEvent, OrderPlaced)]
#[derive(Debug, Clone, Serialize, Deserialize, EventData)]
pub enum AppEvent { /* ... */ }
```

### Correlation & Causation

Every event can carry two tracing fields:

- **`correlation_id`** — shared by all events in the same causal tree (one user action → many events)
- **`causation_id`** — points to the specific event that caused this one

These are set automatically. For commands from external entry points (e.g., HTTP handlers), the aggregate auto-generates a `correlation_id` from the first event's ID. For saga-dispatched commands, use `caused_by` to thread the context:

```rust
// In a saga's handle_event:
let cmd = Command::new(order_id, ReserveItems { .. }, None, None)
    .caused_by(event);  // sets causation_id = event.id, inherits correlation_id

// At system entry points, inject an external trace ID:
let cmd = Command::new(order_id, PlaceOrder { .. }, None, None)
    .with_correlation_id(trace_id);
```

Use `extract_causation_subtree` to navigate the causal tree — given a target event and all correlated events, it returns ancestors + the target + all descendants, ordered by `global_sequence`.

## Aggregates (Write Model)

Aggregates encapsulate business logic, validate commands, enforce invariants, and emit events. They implement both `EventApplicator` (for state reconstruction) and `Aggregate` (for command handling).

```rust
#[async_trait]
impl Aggregate<AppEvent> for UserAggregate {
    type Command = UserCommand;
    // ...
    async fn handle_command(
        &self,
        state: &Option<Self::State>,
        command: Command<Self::Command, Self::CommandCredentials>,
    ) -> Result<Vec<Event<AppEvent>>, Self::AggregateError> {
        match command.data {
            UserCommand::Create { name } => {
                Ok(vec![AppEvent::UserCreated { name }
                    .into_builder()
                    .stream_id(command.aggregate_id)
                    .build()?])
            }
        }
    }
}
```

The `handle` method orchestrates the full lifecycle: load state → validate command → store events → update state → publish to bus.

### Transactions

`TransactionalAggregate` wraps multiple commands in a single database transaction. Events and state are committed atomically, and events are only published after commit:

```rust
let aggregate = Arc::new(my_aggregate);
let mut tx = aggregate.clone().begin().await?;

tx.handle(create_cmd).await?;
tx.handle(update_cmd).await?;  // version tracking works across handles

tx.commit().await?;   // single fsync, then events published
// or tx.rollback().await?;
```

Key points:

- **Atomic** — all events and state changes commit or rollback together
- **Version continuity** — multiple `handle()` calls on the same aggregate within a transaction track versions correctly via an internal cache
- **Events buffered** — published only after successful commit
- **PostgreSQL** — uses `SELECT ... FOR UPDATE` for row-level locking to prevent concurrent modifications

### Event Purging

Events support GDPR/compliance purging. When purged, `data` becomes `None` and `purger_id`/`purged_at` are set. The event metadata (ID, stream, type, timestamps) remains for audit:

```rust
// A purged event
assert!(event.data.is_none());
assert!(event.purger_id.is_some());
assert!(event.purged_at.is_some());
```

## Versioned Snapshot Store

While Epoch's default model persists live state after every command (O(1) reads, no replay), some use cases need access to **historical state** at a specific past version — for audits, time-travel queries, or debugging. The versioned snapshot store provides this without changing normal write-path performance.

Snapshots are distinct from the live `StateStore`: they are version-keyed historical copies, stored in a separate backend, and never affect aggregate command handling.

### Core types

```rust
// Configure capture frequency and retention per aggregate.
let config = SnapshotConfig {
    trigger: SnapshotTrigger::Automatic { interval: 10 }, // capture every 10 events
    retention: SnapshotRetention::KeepLast(5),           // keep the 5 newest snapshots
};

// SnapshotTrigger::Manual disables automatic capture; use save_snapshot() explicitly.
// SnapshotRetention::Unlimited keeps every snapshot ever taken.
```

### `SnapshotStore<S>` trait

```rust
#[async_trait]
pub trait SnapshotStore<S>: Send + Sync {
    // Load the nearest snapshot at or before target_version.
    async fn load_snapshot(&self, stream_id: Uuid, target_version: u64) -> Result<Option<Snapshot<S>>, _>;
    // Save a snapshot (idempotent per (stream_id, version)).
    async fn save_snapshot(&self, stream_id: Uuid, version: u64, state: &S) -> Result<(), _>;
    // Prune snapshots beyond what the retention policy allows.
    async fn apply_retention(&self, stream_id: Uuid, policy: &SnapshotRetention) -> Result<(), _>;
}
```

Available implementations: `InMemorySnapshotStore` (`epoch_mem`, testing) and `PgSnapshotStore` (`epoch_pg`, production).

### Automatic capture with `SnapshottingAggregate`

`SnapshottingAggregate<ED>` is an opt-in extension trait. Implement it on your aggregate, then wire `after_persist` to trigger automatic capture:

```rust
impl SnapshottingAggregate<AppEvent> for UserAggregate {
    type SnapshotStore = InMemorySnapshotStore<UserState>;

    fn snapshot_store(&self) -> Self::SnapshotStore {
        self.snapshot_store.clone()
    }

    fn snapshot_config(&self) -> &SnapshotConfig {
        &self.config  // SnapshotConfig { trigger, retention }
    }
}

// Override the lifecycle hook in your Aggregate impl:
#[async_trait]
impl Aggregate<AppEvent> for UserAggregate {
    // ... other methods ...

    async fn after_persist(
        &self,
        stream_id: Uuid,
        new_version: u64,
        events_applied: usize,
        state: &UserState,
    ) {
        // Captures iff an interval boundary was crossed; logs and swallows store failures.
        self.capture_snapshot_if_due(stream_id, new_version, events_applied, state)
            .await;
    }
}
```

Aggregates that do not implement `SnapshottingAggregate` keep the default no-op `after_persist` — no behaviour change and no overhead.

### Historical state reconstruction with `state_at`

`state_at` reconstructs the state of a stream at an arbitrary past version. It finds the nearest snapshot `≤ version` and replays only the bounded event range `(snapshot_version, version]` — much cheaper than a full replay from version 1:

```rust
use epoch::prelude::state_at;

// Reconstruct state at version 42 — uses snapshot@40 + replay events 41..=42.
let state = state_at(&applicator, &event_store, &snapshot_store, stream_id, 42)
    .await?
    .expect("stream has events up to version 42");
```

If no snapshot exists before `version`, it falls back to a full replay from event 1.

### Manual snapshots

```rust
// Reads the current live state and persists it as a versioned snapshot.
aggregate.save_snapshot(aggregate_id).await?;
```

See [`examples/versioned-snapshots.rs`](../epoch/examples/versioned-snapshots.rs) for a complete runnable example.

## Projections (Read Model)

Projections build denormalized, query-optimized views from events. They are passive — no commands, no business rules, no new events.

The core logic lives in `EventApplicator::apply`, which projections implement:

```rust
impl EventApplicator<AppEvent> for ProductProjection {
    type State = Product;
    type EventType = ProductEvent;  // subset — only receives relevant events
    // ...

    fn apply(
        &self,
        state: Option<Self::State>,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::ApplyError> {
        match event.data.as_ref().unwrap() {
            ProductEvent::Created { name, price } => Ok(Some(Product {
                id: event.stream_id,
                name: name.clone(),
                price: *price,
            })),
            ProductEvent::PriceUpdated { price } => {
                let mut state = state.unwrap();
                state.price = *price;
                Ok(Some(state))
            }
        }
    }
}

impl Projection<AppEvent> for ProductProjection {}

event_bus.subscribe(ProjectionHandler::new(projection)).await?;
```

Key points:

- **`EventApplicator`** defines `apply` — shared base trait between projections and aggregates
- **`Projection`** adds `apply_and_store` for event bus integration (has a default implementation)
- **`ProjectionHandler`** wraps a projection to subscribe it to the event bus
- **Returning `None`** from `apply` deletes the state (e.g., on a deletion event)
- **Non-matching events** are silently ignored — projections only process their subset

## Sagas

Sagas coordinate long-running business processes across multiple aggregates. They maintain a state machine, react to events, and dispatch commands to drive the process forward.

```rust
#[async_trait]
impl Saga<AppEvent> for OrderFulfillmentSaga {
    type State = FulfillmentState;  // enum state machine
    type EventType = OrderFulfillmentEvent;  // subset of events this saga cares about
    // ...

    fn get_id_from_event(&self, event: &Event<Self::EventType>) -> Uuid {
        // Extract saga instance ID from event (e.g., order_id)
    }

    async fn handle_event(
        &self,
        state: Self::State,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::SagaError> {
        match (&state, event.data.as_ref().unwrap()) {
            (FulfillmentState::Pending, OrderFulfillmentEvent::OrderPlaced { .. }) => {
                // Dispatch command to inventory aggregate
                self.inventory.handle(reserve_command).await?;
                Ok(Some(FulfillmentState::InventoryReserved { order_id }))
            }
            (FulfillmentState::InventoryReserved { .. }, OrderFulfillmentEvent::ItemsReserved { .. }) => {
                // Dispatch command to payment aggregate
                self.payment.handle(payment_command).await?;
                Ok(Some(FulfillmentState::PaymentProcessed { order_id }))
            }
            // ... state machine continues
        }
    }
}

event_bus.subscribe(SagaHandler::new(saga)).await?;
```

Key points:

- **State is an enum** acting as a state machine — each variant represents a step in the process
- **`get_id_from_event`** maps events to saga instances (e.g., by `order_id`)
- **State is persisted** automatically after each transition via the state store
- **Returning `None`** deletes the saga state (cleanup after completion or failure)

## Schema Evolution

Event sourcing stores facts forever — once an event is persisted, its serialised JSON shape is frozen. When the Rust type it deserializes into changes (renamed field, new required field, type change), stored old-format records can no longer deserialize cleanly. **Upcasting** solves this by transforming stored JSON one version at a time before deserialization.

Enable the feature (adds `serde_json` as a dependency):

```toml
epoch = { version = "0.1", features = ["upcasting"] }
```

### Writing an upcaster

Each `Upcaster` handles one version step for one event type: v`N` → v`N+1`.

```rust
struct OrderPlacedV1ToV2;

impl Upcaster for OrderPlacedV1ToV2 {
    fn event_type(&self) -> &str { "OrderPlaced" }
    fn from_version(&self) -> SchemaVersion { 1 }

    fn upcast(&self, _ctx: &UpcastContext<'_>, mut payload: Value) -> Result<Value, UpcastError> {
        // Add the new required field with a default.
        if let Value::Object(map) = &mut payload {
            map.entry("currency").or_insert_with(|| json!("USD"));
        }
        Ok(payload)
    }
}
```

Mark the current version on the event type:

```rust
impl EventData for OrderPlaced {
    fn event_type(&self) -> &'static str { "OrderPlaced" }
    fn schema_version(&self) -> SchemaVersion { 2 }  // was 1 before the change
}
// — or via derive:
#[event_data(schema_version = 2)]
#[derive(Debug, Clone, Serialize, Deserialize, EventData)]
pub enum AppEvent { OrderPlaced { product: String, currency: String }, /* ... */ }
```

### Registering with the event store

```rust
// In-memory or unit tests — call upcast_and_deserialize directly.
let mut registry = UpcasterRegistry::new();
registry.register(OrderPlacedV1ToV2);

// PostgreSQL — wire the registry into the read path.
let event_store = PgEventStore::with_upcasters(pool, bus, Arc::new(registry));
```

### Failure policy

```rust
// Default: Fail — abort the stream on any upcasting/deserialization error (safe default).
registry.with_policy(FailurePolicy::Fail);

// Alternative: DeadLetter — route the broken event to a sink and continue the stream.
registry
    .with_policy(FailurePolicy::DeadLetter)
    .with_dead_letter_sink(my_sink);  // impl DeadLetterSink
```

`UpcastError::FutureVersion` (the running binary is older than the data it's reading) always propagates as `Err`, even under `DeadLetter` — silent skipping would hide a serious deploy mismatch.

See [`examples/schema-evolution.rs`](../epoch/examples/schema-evolution.rs) for a runnable example covering single-step, chained, and dead-letter scenarios. The full design guide is at [`docs/schema-evolution.md`](schema-evolution.md).

## Event Store & Event Bus

- **EventStoreBackend** — append-only persistence for events
- **StateStoreBackend** — persists aggregate/projection state
- **EventBus** — publishes events to subscribers (projections, sagas)

The PostgreSQL implementation uses database `NOTIFY` for the event bus, triggered after event persistence. This guarantees only committed events are propagated.

### Bounded event reads with `read_events_range`

The `EventStoreBackend` trait exposes a bounded-replay primitive that pushes version bounds down to storage:

```rust
// Fetch events [from, to] inclusive (both bounds optional).
let stream = event_store.read_events_range(stream_id, Some(5), Some(10)).await?;

// read_events and read_events_since are default methods built on top of this.
```

Backends implement `read_events_range`; `read_events` and `read_events_since` become default implementations that delegate to it. This eliminates the full-stream over-read previously required for upper-bounded replay (e.g., in `state_at`).

### Reliable Delivery (PostgreSQL)

The Pg event bus provides reliable delivery with:

- **Checkpointing** — tracks last processed event per subscriber. Two modes:
  - `Synchronous` (default) — checkpoint after every event, at-most-once redelivery on crash
  - `Batched` — checkpoint every N events or after a time window, better throughput but up to N redeliveries on crash
- **Retry with backoff** — failed events are retried with exponential backoff up to `max_retries`
- **Dead letter queue** — events that exhaust retries are moved to a DLQ for manual inspection
- **Catch-up on startup** — subscribers replay missed events from their checkpoint before processing live events
- **Multi-instance coordination** — `InstanceMode::Coordinated` uses PostgreSQL advisory locks so only one instance per subscriber processes events, with automatic failover

## Crate Structure

| Crate | Purpose |
|-------|---------|
| `epoch` | Main crate, re-exports with feature flags |
| `epoch_core` | Core traits and abstractions |
| `epoch_derive` | Proc macros (`EventData`, `subset_enum`) |
| `epoch_mem` | In-memory implementations (testing) |
| `epoch_pg` | PostgreSQL implementations (production) |

## Design Decisions

**Why persist state on every write?** Trades slightly more write overhead for O(1) reads and always-consistent live state. Historical queries that need state at a past version can use the opt-in versioned snapshot store without affecting the normal read/write path.

**Why do Aggregates and Projections share `EventApplicator`?** Both need to apply events to state. `EventApplicator` captures this shared behavior while keeping the two separate — aggregates can't accidentally be subscribed to the event bus as projections.

**Why both Aggregates and Projections?** Different responsibilities. Aggregates own business logic and invariants. Projections are passive read-model builders. Merging them breaks CQRS separation.

**Why no AggregateRepository?** With immediate state persistence, there's no complex loading logic to encapsulate. The `Aggregate` trait handles the full command lifecycle.

**Why PostgreSQL NOTIFY for the event bus?** Transactional consistency (only committed events propagate), simpler ops (no separate message queue). Trade-off: at-most-once delivery — projections can rebuild from the event store if needed.

## Further Reading

- [Schema Evolution Guide](schema-evolution.md) — full reference for breaking event-type changes: field renames, type changes, required field additions, failure policies, and the split/merge pattern.
