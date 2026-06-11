# Spec 0017 — epoch_pg: Remove Panicking `unwrap()` from Library Read/Checkpoint Paths

**Linear:** CLOUD-170  
**Status:** Approved

## Problem

`epoch_pg` contains several `unwrap()` calls in non-test library code paths that violate the
project's no-unwrap convention and create real operational risk:

| File | Location | Call | Risk |
|------|----------|------|------|
| `epoch_pg/src/event_store.rs` | `pg_db_event_to_event` fn | `entry.stream_version.try_into().unwrap()` | A single negative/corrupt row panics every read of that stream; aggregate becomes permanently un-rehydratable and the bus catch-up loop crash-loops |
| `epoch_pg/src/event_bus/mod.rs` | `should_flush` block | `pending_checkpoint.take().unwrap()` | Guarded by `if should_flush` which checks `as_ref()`, but still technically panics if the guard is wrong |
| `epoch_pg/src/event_bus/mod.rs` | listener reconnect arm | `listener_option.as_mut().unwrap()` | Just set to `Some(l)`, but still an unwrap |
| `epoch_pg/src/event_bus/mod.rs` | three catch-up/buffer flush blocks | `pending_checkpoint.take().unwrap()` | Each guarded by `if let Some(ref pending) = pending_checkpoint && …`, but still panics if reasoning is wrong |

## Proposed Changes

### 1. `epoch_pg/src/event_store.rs` — New error variant + fix `pg_db_event_to_event`

Add a new variant to `PgEventStoreError<BE>`:

```rust
/// A stored `stream_version` is negative and cannot be converted to `u64`.
#[error("Invalid stream_version {0}: value is negative (data corruption)")]
InvalidStreamVersion(i64),
```

Replace the unwrap in `pg_db_event_to_event`:

```rust
// Before
.stream_version(entry.stream_version.try_into().unwrap())

// After
.stream_version(
    entry.stream_version
        .try_into()
        .map_err(|_| PgEventStoreError::InvalidStreamVersion::<BE>(entry.stream_version))?,
)
```

### 2. `epoch_pg/src/event_bus/mod.rs` — `pending_checkpoint.take()` (simple guard)

```rust
// Before
if should_flush {
    let pending = pending_checkpoint.take().unwrap();
    // ... body uses `pending`, may reassign pending_checkpoint
}

// After
if should_flush {
    if let Some(pending) = pending_checkpoint.take() {
        // ... same body
    }
}
```

### 3. `epoch_pg/src/event_bus/mod.rs` — `listener_option.as_mut().unwrap()`

```rust
// Before
listener_option = Some(l);
listener_option.as_mut().unwrap()

// After
listener_option.insert(l)
```

`Option::insert` sets the value and returns `&mut T`, eliminating the unwrap entirely.

### 4. `epoch_pg/src/event_bus/mod.rs` — three `pending_checkpoint.take().unwrap()` (let-chain guards)

Three identical blocks pattern:

```rust
// Before
if let Some(ref pending) = pending_checkpoint
    && should_flush_checkpoint(pending, &config.checkpoint_mode)
{
    let pending = pending_checkpoint.take().unwrap();
    if let Err(e) = flush_checkpoint(...).await {
        // ...
        pending_checkpoint = Some(pending);
    }
}

// After
if let Some(ref pending) = pending_checkpoint
    && should_flush_checkpoint(pending, &config.checkpoint_mode)
{
    if let Some(pending) = pending_checkpoint.take() {
        if let Err(e) = flush_checkpoint(...).await {
            // ...
            pending_checkpoint = Some(pending);
        }
    }
}
```

The inner `if let Some` is logically always true (outer guard confirmed it), but it removes the panic class with no runtime cost.

## New Test

Add a unit test to `epoch_pg/src/event_store.rs` (in the existing `#[cfg(test)]` block):

```rust
#[test]
fn pg_db_event_to_event_rejects_negative_stream_version() {
    let entry = PgDBEvent {
        stream_version: -1,
        // ... minimal valid fields
    };
    let result = pg_db_event_to_event::<TestEvent, std::convert::Infallible>(entry);
    assert!(matches!(result, Err(PgEventStoreError::InvalidStreamVersion(-1))));
}
```

## Acceptance Criteria

- [ ] No `unwrap()` or `expect()` in `epoch_pg` non-test code paths
- [ ] `pg_db_event_to_event` with `stream_version = -1` returns `Err(InvalidStreamVersion(-1))`, not a panic
- [ ] `cargo test -p epoch_pg` passes
- [ ] `cargo clippy -p epoch_pg -- -D warnings` clean
