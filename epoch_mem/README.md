# epoch_mem

`epoch_mem` provides in-memory implementations of the Epoch storage backends, intended for testing and development. All state lives in `Arc<Mutex<...>>` so instances can be cheaply cloned and shared across test fixtures.

> **Not for production.** All data is lost when the process exits.

## Provided types

| Type | Implements | Use |
|------|-----------|-----|
| `InMemoryEventStore<Bus>` | `EventStoreBackend` | Stores events; drives `InMemoryEventBus` on write |
| `InMemoryEventBus<ED>` | `EventBus` | Delivers events to in-process subscribers |
| `InMemoryStateStore<S>` | `StateStoreBackend` | Persists live aggregate and projection state |
| `InMemorySnapshotStore<S>` | `SnapshotStore<S>` | Stores version-keyed historical snapshots for `state_at` and `SnapshottingAggregate` |

## Usage

```rust
use epoch_mem::{InMemoryEventBus, InMemoryEventStore, InMemoryStateStore, InMemorySnapshotStore};

let bus = InMemoryEventBus::<AppEvent>::new();
let event_store = InMemoryEventStore::new(bus);
let state_store = InMemoryStateStore::<UserState>::new();

// Optional: versioned snapshot store for state_at / SnapshottingAggregate
let snapshot_store = InMemorySnapshotStore::<UserState>::new();
```

All types implement `Clone` via inner `Arc`, so multiple handles share the same underlying store — useful for asserting state in tests without extra ceremony.

## `InMemorySnapshotStore<S>`

Implements `SnapshotStore<S>` (where `S: Clone + Send + Sync`). Snapshots are kept sorted by version; `load_snapshot` returns the nearest entry `≤ target_version`:

```rust
use epoch_core::prelude::*;
use epoch_mem::InMemorySnapshotStore;

let store = InMemorySnapshotStore::<MyState>::new();

// Save and load
store.save_snapshot(stream_id, 5, &state_at_v5).await?;
let snap = store.load_snapshot(stream_id, 7).await?; // returns the v5 snapshot

// Prune to keep only the 2 most recent
store.apply_retention(stream_id, &SnapshotRetention::KeepLast(2)).await?;
```

`save_snapshot` is idempotent per `(stream_id, version)`: re-saving the same version overwrites the stored state without error.
