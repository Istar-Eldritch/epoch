# epoch_core

`epoch_core` is the foundational crate for the Epoch ecosystem. It defines the core traits and types for building event-sourced systems using CQRS patterns: events, aggregates, projections, sagas, the event store and bus interfaces, versioned snapshots, and event schema evolution via upcasting.

## Features

- **Events** — `EventData` trait, `Event<D>` envelope, correlation/causation tracing, event purging
- **Aggregates** — `Aggregate<ED>` trait, command handling lifecycle, optimistic concurrency, `TransactionalAggregate`
- **Projections** — `Projection<ED>` trait, `ProjectionHandler` for event bus subscription
- **Sagas** — `Saga<ED>` trait, `SagaHandler` for coordinating long-running processes
- **Event Store** — `EventStoreBackend` trait with `read_events_range` bounded-replay primitive; `read_events` and `read_events_since` are default methods built on top
- **State Store** — `StateStoreBackend` trait for persisting live aggregate and projection state
- **Versioned Snapshot Store** — `SnapshotStore<S>` trait, `SnapshottingAggregate<ED>` extension trait, `state_at` for historical state reconstruction; enabled for all builds (no feature flag)
- **Schema Evolution** — `Upcaster` trait, `UpcasterRegistry`, `FailurePolicy`, `DeadLetterSink` for forward-only JSON schema migration at the deserialization boundary; gated behind the `upcasting` feature flag
- **Contract testing** — `verify_store_events_atomicity` and `verify_upcasting_chain` helpers for testing backend implementations; gated behind the `testing` feature flag

## Cargo features

```toml
# Schema evolution / upcasting (adds serde_json dependency)
epoch_core = { version = "0.1", features = ["upcasting"] }

# Contract test helpers (for testing your own EventStoreBackend impl)
epoch_core = { version = "0.1", features = ["testing"] }
```

## Core traits

### `EventData`

All domain events implement `EventData`. Use `#[derive(EventData)]` from `epoch_derive` for enums:

```rust
#[derive(Debug, Clone, Serialize, Deserialize, EventData)]
pub enum AppEvent {
    UserCreated { name: String },
    OrderPlaced { product: String },
}
```

For schema evolution, mark the current version:

```rust
#[event_data(schema_version = 2)]
#[derive(Debug, Clone, Serialize, Deserialize, EventData)]
pub enum AppEvent { /* ... */ }
```

### `Aggregate<ED>`

Aggregates validate commands, enforce invariants, and emit events:

```rust
#[async_trait]
impl Aggregate<AppEvent> for UserAggregate {
    type CommandData = AppCommand;
    type Command = UserCommand;
    type AggregateError = UserError;
    type EventStore = /* ... */;
    // ...

    async fn handle_command(
        &self,
        state: &Option<Self::State>,
        command: Command<Self::Command, Self::CommandCredentials>,
    ) -> Result<Vec<Event<AppEvent>>, Self::AggregateError> { /* ... */ }
}
```

`handle()` orchestrates the full lifecycle: load state → validate → store events → update state → publish to bus.

### `SnapshotStore<S>` and `SnapshottingAggregate<ED>`

The versioned snapshot store is an opt-in layer for historical state reconstruction, distinct from the live `StateStoreBackend`:

```rust
// Configure capture and retention
let config = SnapshotConfig {
    trigger: SnapshotTrigger::Automatic { interval: 10 },
    retention: SnapshotRetention::KeepLast(5),
};

// Implement the extension trait on your aggregate
impl SnapshottingAggregate<AppEvent> for UserAggregate {
    type SnapshotStore = /* InMemorySnapshotStore or PgSnapshotStore */;

    fn snapshot_store(&self) -> Self::SnapshotStore { /* ... */ }
    fn snapshot_config(&self) -> &SnapshotConfig { &self.config }
}

// Wire into the lifecycle hook
async fn after_persist(&self, stream_id: Uuid, new_version: u64, events_applied: usize, state: &UserState) {
    self.capture_snapshot_if_due(stream_id, new_version, events_applied, state).await;
}

// Reconstruct state at any past version
let state = state_at(&applicator, &event_store, &snapshot_store, stream_id, 42).await?;
```

### `Projection<ED>`

Projections subscribe to the event bus and build denormalized read models:

```rust
impl EventApplicator<AppEvent> for MyProjection {
    type EventType = MyEvent; // subset — only receives relevant events

    fn apply(&self, state: Option<Self::State>, event: &Event<Self::EventType>)
        -> Result<Option<Self::State>, Self::ApplyError> { /* ... */ }
}

impl Projection<AppEvent> for MyProjection {}

event_bus.subscribe(ProjectionHandler::new(projection)).await?;
```

### `Saga<ED>`

Sagas coordinate long-running processes by reacting to events and dispatching commands:

```rust
#[async_trait]
impl Saga<AppEvent> for OrderFulfillmentSaga {
    type State = FulfillmentState;
    type EventType = OrderFulfillmentEvent;

    async fn handle_event(
        &self,
        state: Self::State,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::SagaError> { /* ... */ }
}

event_bus.subscribe(SagaHandler::new(saga)).await?;
```

### Event schema evolution (`upcasting` feature)

Register forward-only transformations that run at the deserialization boundary:

```rust
struct OrderPlacedV1ToV2;

impl Upcaster for OrderPlacedV1ToV2 {
    fn event_type(&self) -> &str { "OrderPlaced" }
    fn from_version(&self) -> SchemaVersion { 1 }

    fn upcast(&self, _ctx: &UpcastContext<'_>, mut payload: Value) -> Result<Value, UpcastError> {
        if let Value::Object(map) = &mut payload {
            map.entry("currency").or_insert_with(|| json!("USD"));
        }
        Ok(payload)
    }
}

let mut registry = UpcasterRegistry::new();
registry.register(OrderPlacedV1ToV2);
```

See [Schema Evolution Guide](../docs/schema-evolution.md) for the full reference.

## Subset event types

Use `#[subset_enum]` (from `epoch_derive`) to scope projections and sagas to only the variants they care about:

```rust
#[subset_enum(UserEvent, UserCreated, UserUpdated)]
#[subset_enum(OrderEvent, OrderPlaced, OrderShipped)]
#[derive(Debug, Clone, Serialize, Deserialize, EventData)]
pub enum AppEvent { /* all variants */ }

// UserProjection only receives UserCreated and UserUpdated
impl EventApplicator<AppEvent> for UserProjection {
    type EventType = UserEvent;
    // ...
}
```

For aggregates or applicators where `EventType == ED` (no subset), implement the identity conversion manually:

```rust
impl TryFrom<&MyEvent> for MyEvent {
    type Error = EnumConversionError;
    fn try_from(v: &MyEvent) -> Result<Self, Self::Error> { Ok(v.clone()) }
}
```
