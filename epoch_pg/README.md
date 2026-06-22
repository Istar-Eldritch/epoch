# epoch_pg

`epoch_pg` provides PostgreSQL-backed implementations of the Epoch storage backends, powered by `sqlx`. This is the production crate.

## Provided types

| Type | Implements | Use |
|------|-----------|-----|
| `PgEventStore<Bus>` | `EventStoreBackend` | Append-only event persistence with schema-version and txid columns |
| `PgEventBus` | `EventBus` | Reliable event delivery via PostgreSQL `LISTEN`/`NOTIFY` with checkpointing, retries, and DLQ |
| `PgStateStore<S>` | `StateStoreBackend` | JSON-serialised live aggregate/projection state |
| `PgSnapshotStore<S>` | `SnapshotStore<S>` | Version-keyed historical snapshots; requires `S: Serialize + DeserializeOwned` |

## Schema migrations

`epoch_pg` manages its own schema via versioned, forward-only migrations applied automatically at startup. Current migrations include:

- `epoch_events` table with `stream_id`, `stream_version`, `event_type`, `schema_version`, `txid`, and `data jsonb` columns
- `epoch_state` table for live aggregate/projection state
- `epoch_snapshots` table (`PRIMARY KEY (stream_id, version)`) for versioned snapshots
- `epoch_event_bus_*` tables for subscriber checkpoints, DLQ entries, and gap-timeout records

## `PgSnapshotStore<S>`

```rust
use epoch_pg::PgSnapshotStore;

let snapshot_store = PgSnapshotStore::<UserState>::new(pool.clone());

// Wire into a SnapshottingAggregate
impl SnapshottingAggregate<AppEvent> for UserAggregate {
    type SnapshotStore = PgSnapshotStore<UserState>;

    fn snapshot_store(&self) -> Self::SnapshotStore {
        PgSnapshotStore::new(self.pool.clone())
    }
    fn snapshot_config(&self) -> &SnapshotConfig { &self.config }
}
```

State type `S` must implement `serde::Serialize + serde::de::DeserializeOwned`. Snapshots are stored as `jsonb` in `epoch_snapshots`.

## Schema evolution / upcasting

`PgEventStore` reads the `schema_version` column written at insert time and passes it through `UpcasterRegistry` before deserialization. Wire a registry at construction:

```rust
use std::sync::Arc;
use epoch_pg::PgEventStore;
use epoch_core::upcasting::UpcasterRegistry;

let mut registry = UpcasterRegistry::new();
registry.register(OrderPlacedV1ToV2);

let event_store = PgEventStore::with_upcasters(pool, bus, Arc::new(registry));
```

Without `with_upcasters`, the store uses an empty registry — events that still deserialize cleanly behave exactly as before.

## Reliable event delivery

`PgEventBus` provides production-grade delivery guarantees:

- **Checkpointing** — tracks last processed `global_sequence` per subscriber (`Synchronous` or `Batched` mode)
- **Catch-up on startup** — replays missed events from the checkpoint before processing live ones
- **Retry with backoff** — configurable `max_retries` and exponential backoff
- **Dead letter queue** — events that exhaust retries land in `epoch_event_bus_dlq` for manual inspection
- **Gap detection with snapshot fencing** — uses `pg_current_xact_id` to distinguish in-flight transactions from permanent gaps (PostgreSQL 13+ required for the event bus)
- **Multi-instance coordination** — `InstanceMode::Coordinated` uses advisory locks so only one process per subscriber handles events at a time, with automatic failover

## Running the tests

Start the Docker container defined in `docker-compose.yml`, then run tests single-threaded to avoid cross-test interference:

```bash
cargo test -p epoch_pg -- --test-threads=1
```
