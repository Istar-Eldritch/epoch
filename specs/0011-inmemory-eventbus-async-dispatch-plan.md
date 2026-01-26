# Implementation Plan: InMemoryEventBus Async Dispatch

## Overview

Step-by-step plan to implement channel-based async dispatch for `InMemoryEventBus`.

## Implementation Steps

### Step 1: Update InMemoryEventBus struct

**File**: `epoch_mem/src/event_store.rs`

**Actions**:
1. Add `event_tx` field: `tokio::sync::mpsc::UnboundedSender<Arc<Event<D>>>`
2. Add `_listener_handle` field: `Arc<tokio::task::JoinHandle<()>>`

**Test**: Verify code compiles with `cargo build -p epoch_mem`

---

### Step 2: Update InMemoryEventBus::new() method

**File**: `epoch_mem/src/event_store.rs`

**Actions**:
1. Create unbounded channel: `tokio::sync::mpsc::unbounded_channel()`
2. Clone `projections` Arc for background task
3. Spawn background task with `tokio::spawn`
4. In background task:
   - Loop: `while let Some(event) = event_rx.recv().await`
   - Get observers: `projections.read().await`
   - Iterate and call `observer.on_event(event).await`
   - Log errors
5. Store `event_tx` and `listener_handle` in struct
6. Add debug logging

**Test**: Verify code compiles with `cargo build -p epoch_mem`

---

### Step 3: Update InMemoryEventBus::publish() method

**File**: `epoch_mem/src/event_store.rs`

**Actions**:
1. Replace the loop with: `self.event_tx.send(event)?`
2. Keep debug logging
3. Return `Ok(())`

**Test**: Verify code compiles with `cargo build -p epoch_mem`

---

### Step 4: Update InMemoryEventBusError enum

**File**: `epoch_mem/src/event_store.rs`

**Actions**:
1. Change `PublishError` to use `tokio::sync::mpsc::error::SendError<Arc<Event<D>>>`
2. Remove `PublishTimeoutError` variant (not needed with unbounded channel)

**Test**: Verify code compiles with `cargo build -p epoch_mem`

---

### Step 5: Run existing tests

**Actions**:
1. Run `cargo test -p epoch_mem`
2. Fix any test failures related to timing/async behavior
3. May need to add small delays in tests to allow background task to process

**Test**: All tests in `epoch_mem` should pass

---

### Step 6: Test with saga example (before changes)

**Actions**:
1. Run `RUST_LOG=info cargo run --example saga-order-fulfillment`
2. Verify it still works with manual spawning
3. Check for any timing changes

**Test**: Example should run successfully and show completed saga

---

### Step 7: Simplify saga - remove first manual spawn

**File**: `epoch/examples/saga-order-fulfillment.rs`

**Actions**:
1. Find the first `tokio::spawn` in saga (OrderPlaced → ReserveItems)
2. Replace with direct `.await` call
3. Change error handling from `log::error` to `?` operator

**Test**: Run example and verify it still works

---

### Step 8: Simplify saga - remove second manual spawn

**File**: `epoch/examples/saga-order-fulfillment.rs`

**Actions**:
1. Find the second `tokio::spawn` (ItemsReserved → ProcessPayment)
2. Replace with direct `.await` call
3. Change error handling

**Test**: Run example and verify it still works

---

### Step 9: Simplify saga - remove third manual spawn

**File**: `epoch/examples/saga-order-fulfillment.rs`

**Actions**:
1. Find the third `tokio::spawn` (PaymentProcessed → ConfirmOrder)
2. Replace with direct `.await` call
3. Change error handling

**Test**: Run example and verify complete workflow

---

### Step 10: Update saga example documentation

**File**: `epoch/examples/saga-order-fulfillment.rs`

**Actions**:
1. Remove comments about manual spawning
2. Add comment explaining that event bus handles async dispatch
3. Update module documentation if needed

**Test**: Run `cargo doc --no-deps` and verify docs render correctly

---

### Step 11: Add test for async behavior

**File**: `epoch_mem/src/event_store.rs` (in test module)

**Actions**:
1. Add test: `test_publish_returns_immediately`
2. Create a slow observer that sleeps
3. Verify `publish()` returns quickly
4. Verify observer eventually processes event

**Test**: New test should pass

---

### Step 12: Run all tests

**Actions**:
1. Run `cargo test` (all workspace tests)
2. Fix any failures
3. Verify saga example still works

**Test**: All tests pass, example runs successfully

---

### Step 13: Run clippy and fmt

**Actions**:
1. Run `cargo clippy -- -D warnings`
2. Fix any warnings
3. Run `cargo fmt`

**Test**: No clippy warnings, code is formatted

---

### Step 14: Update InMemoryEventBus documentation

**File**: `epoch_mem/src/event_store.rs`

**Actions**:
1. Add module-level doc comment explaining channel-based design
2. Explain similarity to PgEventBus NOTIFY/LISTEN
3. Note that this prevents synchronous call chains

**Test**: Verify documentation is clear and accurate

---

## Summary

The implementation follows a careful incremental approach:
1. Add channel infrastructure (Steps 1-4)
2. Verify existing behavior (Step 5)
3. Incrementally simplify saga (Steps 6-9)
4. Add tests and documentation (Steps 10-14)

Each step can be tested independently to ensure we don't break existing functionality.

## Rollback Plan

If issues arise:
1. The changes are isolated to `epoch_mem/src/event_store.rs`
2. Can revert to synchronous implementation easily
3. Saga example can keep manual spawning as fallback

## Estimated Time

- Steps 1-4: Implement changes (~30 min)
- Steps 5-6: Test existing behavior (~15 min)
- Steps 7-9: Simplify saga (~20 min)
- Steps 10-14: Tests and docs (~25 min)
- **Total**: ~90 minutes
