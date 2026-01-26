# Epoch

A Rust framework for building event-sourced systems with CQRS patterns.

## Overview

Epoch is an **opinionated event sourcing framework** that prioritizes simplicity and performance. It provides the core building blocks for creating scalable, auditable systems with clear separation between write and read models.

### Key Design Philosophy

> **Aggregates are living snapshots.**

Unlike traditional event sourcing where you might replay thousands of events to reconstruct state, Epoch persists aggregate state immediately after every command. This means:

- **O(1) reads** - Loading an aggregate is a single state fetch, not an event replay
- **No snapshotting complexity** - No intervals, thresholds, or snapshot management
- **Always consistent** - State is updated atomically with event storage
- **Full audit trail** - Events are still stored for history, projections, and compliance

## Core Concepts

### Events

Events are immutable facts that describe something that happened. They are the source of truth in an event-sourced system.

```rust
#[derive(Debug, Clone, Serialize, Deserialize, EventData)]
pub enum UserEvent {
    UserCreated { name: String, email: String },
    EmailChanged { new_email: String },
    UserDeleted,
}
```

### Aggregates

Aggregates encapsulate business logic, enforce invariants, and emit events. In Epoch, they also maintain their own persisted state.

```rust
#[async_trait]
impl Aggregate<AppEvent> for UserAggregate {
    type Command = UserCommand;
    type CommandCredentials = ();
    // ...

    async fn handle_command(
        &self,
        state: &Option<Self::State>,
        command: Command<Self::Command, Self::CommandCredentials>,
    ) -> Result<Vec<Event<AppEvent>>, Self::AggregateError> {
        match command.data {
            UserCommand::Create { name, email } => {
                // Validate and emit event
                Ok(vec![UserCreated { name, email }.into_event(command.aggregate_id)])
            }
            // ...
        }
    }
}
```

### Projections

Projections build read-optimized views from event streams. They subscribe to events and maintain denormalized state for querying.

```rust
impl Projection<AppEvent> for UserListProjection {
    type State = UserListState;
    type EventType = UserEvent;
    // ...

    fn apply(
        &self,
        state: Option<Self::State>,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::ProjectionError> {
        // Transform events into queryable state
    }
}

// Subscribe to the event bus using ProjectionHandler
use epoch::ProjectionHandler;
event_bus.subscribe(ProjectionHandler::new(user_list_projection)).await?;
```

### Sagas

Sagas coordinate long-running business processes across multiple aggregates. They react to events and dispatch commands.

```rust
#[async_trait]
impl Saga<AppEvent> for OrderFulfillmentSaga {
    type State = OrderFulfillmentState;
    type EventType = OrderEvent;
    // ...

    async fn handle_event(
        &self,
        state: Self::State,
        event: &Event<Self::EventType>,  // Takes reference for efficiency
    ) -> Result<Option<Self::State>, Self::SagaError> {
        match &event.data {
            OrderEvent::OrderPlaced { .. } => {
                // Dispatch commands to other aggregates
            }
            // ...
        }
    }
}

// Subscribe to the event bus using SagaHandler
use epoch::SagaHandler;
event_bus.subscribe(SagaHandler::new(order_fulfillment_saga)).await?;
```

### Event Store & Event Bus

- **`EventStoreBackend`** - Trait for persisting and reading events
- **`EventBus`** - Trait for publishing events to subscribers
- **`ProjectionHandler`** - Wrapper to subscribe projections to the event bus
- **`SagaHandler`** - Wrapper to subscribe sagas to the event bus

## Crate Structure

| Crate | Description |
|-------|-------------|
| `epoch` | Main crate with re-exports and feature flags |
| `epoch_core` | Core traits and abstractions |
| `epoch_derive` | Proc macros (`#[derive(EventData)]`, `#[subset_enum]`) |
| `epoch_mem` | In-memory implementations for testing |
| `epoch_pg` | PostgreSQL implementations |

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
epoch = { version = "0.1", features = ["derive", "postgres"] }
```

## Examples

- [`epoch/examples/hello-world.rs`](epoch/examples/hello-world.rs) - Basic aggregate and projection usage
- [`epoch/examples/saga-order-fulfillment.rs`](epoch/examples/saga-order-fulfillment.rs) - Saga pattern for coordinating multi-aggregate workflows

## Features

- **`derive`** (default) - Proc macros for event data and enum subsetting
- **`in-memory`** - In-memory event store and bus for testing
- **`postgres`** - PostgreSQL-backed event store and bus

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                         Command                                  │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                        Aggregate                                 │
│  ┌──────────────┐    ┌──────────────┐    ┌──────────────┐      │
│  │ Validate     │───▶│ Emit Events  │───▶│ Apply Events │      │
│  └──────────────┘    └──────────────┘    └──────────────┘      │
└─────────────────────────────────────────────────────────────────┘
                              │
              ┌───────────────┼───────────────┐
              ▼               ▼               ▼
┌──────────────────┐ ┌──────────────┐ ┌──────────────────┐
│   Event Store    │ │ State Store  │ │    Event Bus     │
│ (append events)  │ │(save state)  │ │ (notify)         │
└──────────────────┘ └──────────────┘ └──────────────────┘
                                              │
                              ┌───────────────┼───────────────┐
                              ▼               ▼               ▼
                      ┌─────────────┐ ┌─────────────┐ ┌─────────────┐
                      │ Projection  │ │ Projection  │ │    Saga     │
                      │     A       │ │     B       │ │             │
                      └─────────────┘ └─────────────┘ └─────────────┘
```

## Documentation

- [Architectural Decisions](docs/architectural_points.md) - Design rationale and trade-offs
- [API Documentation](https://docs.rs/epoch) - Generated rustdoc

## License

LGPL-3.0-or-later

