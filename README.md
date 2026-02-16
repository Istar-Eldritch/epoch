# Epoch

A Rust framework for building event-sourced systems with CQRS patterns.

## Overview

Epoch is an **opinionated event sourcing framework** that prioritizes simplicity and performance. The core idea:

> **Aggregates are living snapshots.** State is persisted immediately after every command — no event replay on reads, no snapshot management. Events are still stored for audit, projections, and history.

## Quick Start

```toml
[dependencies]
epoch = { version = "0.1", features = ["derive", "postgres"] }
```

## Features

- **`derive`** (default) — Proc macros for event data and enum subsetting
- **`in-memory`** — In-memory event store and bus for testing
- **`postgres`** — PostgreSQL-backed event store and bus

## Crate Structure

| Crate | Purpose |
|-------|---------|
| `epoch` | Main crate, re-exports with feature flags |
| `epoch_core` | Core traits and abstractions |
| `epoch_derive` | Proc macros (`EventData`, `subset_enum`) |
| `epoch_mem` | In-memory implementations (testing) |
| `epoch_pg` | PostgreSQL implementations (production) |

## Examples

- [`hello-world.rs`](epoch/examples/hello-world.rs) — Basic aggregate and projection usage
- [`saga-order-fulfillment.rs`](epoch/examples/saga-order-fulfillment.rs) — Multi-aggregate saga coordination
- [`event-correlation-causation.rs`](epoch/examples/event-correlation-causation.rs) — Event correlation and causation tracking

## Documentation

- [Guide](docs/guide.md) — Architecture, patterns, and design decisions

## License

LGPL-3.0-or-later
