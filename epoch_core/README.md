# epoch_core

`epoch_core` is the foundational crate for building event-driven applications within the Epoch ecosystem. It provides core traits and types for defining events, managing event streams in an event store, and creating projections and sagas to derive read models and coordinate processes from event data.

## Features

- **Event Definition**: Provides traits and types for defining domain events.
- **Event Store Interface**: Defines the fundamental interface for interacting with an event store, allowing for appending, reading, and subscribing to events.
- **Projections**: Offers mechanisms for creating and managing projections, enabling the transformation of event streams into queryable read models.
- **Sagas**: Provides traits for implementing sagas that coordinate long-running processes across aggregates.
- **Asynchronous Operations**: All APIs are designed for asynchronous event processing.

## Usage

Add `epoch_core` as a dependency in your `Cargo.toml`:

```toml
[dependencies]
epoch_core = { workspace = true }
```

## Core Traits

### Events

Events are immutable facts representing something that happened in the system:

```rust
use epoch_core::prelude::*;

// Define your event data (typically using #[derive(EventData)] from epoch_derive)
pub enum MyEvent {
    Created { name: String },
    Updated { name: String },
    Deleted,
}
```

### Projections

Projections build read-optimized views from events:

```rust
use epoch_core::prelude::*;

impl Projection<AppEvent> for MyProjection {
    type State = MyProjectionState;
    type StateStore = MyStateStore;
    type EventType = MyEvent;
    type ProjectionError = MyError;

    fn apply(
        &self,
        state: Option<Self::State>,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::ProjectionError> {
        // Build read model from events
    }

    fn get_state_store(&self) -> Self::StateStore {
        // Return the state store
    }
}
```

### Sagas

Sagas coordinate long-running business processes:

```rust
use epoch_core::prelude::*;

#[async_trait]
impl Saga<AppEvent> for MySaga {
    type State = MySagaState;
    type StateStore = MyStateStore;
    type SagaError = MyError;
    type EventType = MySagaEvent;

    fn get_state_store(&self) -> Self::StateStore {
        // Return the state store
    }

    async fn handle_event(
        &self,
        state: Self::State,
        event: &Event<Self::EventType>,  // Takes reference for efficiency
    ) -> Result<Option<Self::State>, Self::SagaError> {
        // React to events and coordinate processes
    }
}
```

## Subscribing to the Event Bus

To subscribe projections and sagas to an event bus, use the provided handler wrappers:

```rust
use epoch_core::prelude::*;

// For projections
let projection = MyProjection::new();
event_bus.subscribe(ProjectionHandler::new(projection)).await?;

// For sagas
let saga = MySaga::new();
event_bus.subscribe(SagaHandler::new(saga)).await?;
```

The wrapper types (`ProjectionHandler` and `SagaHandler`) provide efficient `EventObserver` implementations that:
- Receive events as `Arc<Event<ED>>` for zero-cost sharing
- Pass events by reference to avoid unnecessary cloning
- Handle event type conversion automatically

## Subset Event Types

Use the `#[subset_enum]` macro (from `epoch_derive`) to define focused event types for projections and sagas:

```rust
use epoch_derive::subset_enum;

// Define subset enums for different bounded contexts
#[subset_enum(UserEvent, UserCreated, UserUpdated)]
#[subset_enum(OrderEvent, OrderCreated, OrderShipped)]
#[derive(Debug, Clone, Serialize, Deserialize, EventData)]
pub enum ApplicationEvent {
    UserCreated { name: String },
    UserUpdated { name: String },
    UserDeleted,
    OrderCreated { product: String },
    OrderShipped { tracking: String },
}

// UserEvent only contains: UserCreated, UserUpdated
// OrderEvent only contains: OrderCreated, OrderShipped
```

The macro generates:
- The subset enum type
- `From<SubsetEnum>` for converting to the parent enum
- `TryFrom<ParentEnum>` for converting from the parent (may fail)
- `TryFrom<&ParentEnum>` for efficient reference-based conversion

## Event Type Conversion

Projections and sagas use subset event types. The framework automatically converts events:

```rust
impl Projection<ApplicationEvent> for UserProjection {
    type EventType = UserEvent;  // Only receives user-related events
    // ...
}
```

For **identity conversions** (where `EventType == ED`), add this impl:

```rust
impl TryFrom<&MyEvent> for MyEvent {
    type Error = EnumConversionError;
    
    fn try_from(value: &MyEvent) -> Result<Self, Self::Error> {
        Ok(value.clone())
    }
}
```

## Event Ownership Model

Epoch uses a hybrid ownership model optimized for performance:

| Component | Receives | Rationale |
|-----------|----------|-----------|
| `EventStoreBackend::store_event` | `Event<T>` (owned) | Store consumes and persists the event |
| `EventBus::publish` | `Arc<Event<T>>` | Shared across multiple subscribers |
| `EventObserver::on_event` | `Arc<Event<T>>` | Can hold reference or dereference |
| `Projection::apply` | `&Event<T>` | Read-only access |
| `Saga::handle_event` | `&Event<T>` | Read-only access |
| `Saga::process_event` | `&Event<T>` | Read-only access |

This design minimizes cloning while maintaining a clean API.
