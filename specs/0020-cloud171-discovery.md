# CLOUD-171 Discovery: `store_events()` Default Impl Non-Atomic

## Source

Linear issue [CLOUD-171](https://linear.app/catallactical/issue/CLOUD-171/epoch-core-store-events-default-impl-is-non-atomic-enforce-batch)

## Problem

The default `EventStoreBackend::store_events()` implementation at `epoch_core/src/event_store.rs:65-78` loops `store_event()` per event — a partial failure leaves a half-written batch, i.e. corrupted aggregate history.

```rust
async fn store_events(&self, events: Vec<Event<Self::EventType>>) -> Result<(), Self::Error> {
    for event in events {
        self.store_event(event).await?;
    }
    Ok(())
}
```

## Current State

- **`epoch_core/src/event_store.rs:24`** — `EventStoreBackend` trait defines both `store_event()` (line 45) and `store_events()` (line 65) with the non-atomic default
- **`epoch_mem/src/event_store.rs:162`** — Overrides `store_events()` with a two-phase approach: validate all versions first, then store all, then publish. This is atomic on the storage side (lock held during validation + storage) but publishing is best-effort after lock release.
- **`epoch_pg/src/event_store.rs:424`** — Overrides `store_events()` with a PostgreSQL transaction: `BEGIN`, `store_events_in_tx()`, `COMMIT`, then publish. Truly atomic on the storage side.
- **No contract test** exists that custom backends can run to verify atomicity.

## Options (from issue)

1. **Remove the default impl** — breaking change, forces every implementor to make an explicit atomicity decision
2. **Keep default + document + add contract test helper** — non-breaking, but the broken default still exists
3. **Add stream_version contiguity validation** — catches one class of bug but doesn't solve atomicity

## Acceptance Criteria

- Trait docs state the atomicity contract explicitly
- A reusable test (or removed default) prevents custom backends from shipping non-atomic batches unknowingly

## Codebase Map

| File | Lines | Relevance |
|------|-------|-----------|
| `epoch_core/src/event_store.rs` | 24-78 | `EventStoreBackend` trait, `store_event()` (L45), `store_events()` default (L65-78) |
| `epoch_mem/src/event_store.rs` | 162-238 | InMemory override of `store_events()` — two-phase atomic |
| `epoch_pg/src/event_store.rs` | 88-95, 424-440 | Pg `store_events_in_tx()` and `store_events()` override — transactional atomic |
| `epoch_core/src/lib.rs` | — | Public exports that may need updating |
| `epoch_core/tests/` | — | Integration tests location for contract test helpers |
