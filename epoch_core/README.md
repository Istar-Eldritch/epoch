# epoch_core

`epoch_core` is the foundational crate for building event-driven applications within the Epoch ecosystem. It provides core traits and types for defining events, managing event streams in an event store, and creating projections to derive read models from event data. This crate is designed to be a lightweight and flexible base for implementing event-sourcing patterns.

## Features

- **Event Definition**: Provides traits and macros for defining domain events.
- **Event Store Interface**: Defines the fundamental interface for interacting with an event store, allowing for appending, reading, and subscribing to events.
- **Projections**: Offers mechanisms for creating and managing projections, enabling the transformation of event streams into queryable read models.
- **Asynchronous Operations**: We define the APIs for asynchronous event processing.

## Usage

Add `epoch_core` as a dependency in your `Cargo.toml`:

```toml
[dependencies]
epoch_core = { workspace = true }
```

Then, you can start defining your events, implementing event stores, and building projections using the provided traits and types.

