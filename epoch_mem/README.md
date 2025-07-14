# Epoch Memory Store

`epoch_mem` is an in-memory implementation of an event store and event bus, designed primarily for testing and development purposes within the `epoch` project. It provides a non-persistent way to store and retrieve events, and to publish events to subscribed projections.

## Features

- **InMemoryEventStore**: Stores events and manages event streams in memory. Ideal for unit and integration testing where persistence is not required.
- **InMemoryEventBus**: Facilitates in-memory event publishing and subscription, allowing projections to react to events.

## Usage

This crate is part of the larger `epoch` project and is intended for internal use, particularly for testing event-driven architectures without the overhead of a durable storage backend.

## Warning

This crate is **not recommended for production use** as it does not provide any form of durable storage for events. All data will be lost when the application stops.

