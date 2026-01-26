# Specification: InMemoryEventBus Async Dispatch via Channels

## Status: Approved for Implementation

## Overview

Make `InMemoryEventBus` behave like `PgEventBus` by using channels and a background task for event dispatch. This eliminates the synchronous call chain that causes race conditions in sagas and makes the in-memory implementation match production behavior.

## Motivation

### Current Problem

`InMemoryEventBus` currently uses synchronous sequential dispatch:

```rust
fn publish(&self, event: Arc<Event<D>>) -> ... {
    Box::pin(async move {
        let observers = self.observers.read().await;
        for observer in observers.iter() {
            observer.on_event(Arc::clone(&event)).await;  // BLOCKS
        }
        Ok(())
    })
}
```

This creates a **synchronous call chain**:
1. Aggregate handles command → stores event → publishes to bus
2. Bus immediately calls observer.on_event() **before returning**
3. Saga handles event → dispatches command → stores event → publishes
4. Saga receives new event **before its state from step 3 is persisted**

In contrast, `PgEventBus`:
- `publish()` is a NO-OP that returns immediately
- Events are delivered via NOTIFY/LISTEN asynchronously
- Background task processes events from queue
- No nested call chains

### The Solution

Make `InMemoryEventBus` use the same pattern:
- `publish()` sends event to a channel and returns immediately
- Background task receives events from channel
- Background task dispatches to observers sequentially (maintaining order)
- Breaks the synchronous call chain

## Goals

1. **Match Production Behavior**: InMemoryEventBus should work like PgEventBus
2. **Fix Saga Race Conditions**: No more manual `tokio::spawn` workarounds needed
3. **Maintain Order**: Events still processed sequentially within the background task
4. **Backward Compatibility**: Existing tests and code should work without changes

## Non-Goals

- Concurrent observer dispatch (maintaining sequential processing)
- Changing the `EventBus` trait
- Modifying `PgEventBus`

## Proposed Changes

### File: `epoch_mem/src/event_store.rs`

#### 1. Update `InMemoryEventBus` structure

**Current**:
```rust
pub struct InMemoryEventBus<D>
where
    D: EventData + Send + Sync + 'static,
{
    _phantom: PhantomData<D>,
    projections: Arc<RwLock<Vec<Box<dyn EventObserver<D>>>>>,
}
```

**New**:
```rust
pub struct InMemoryEventBus<D>
where
    D: EventData + Send + Sync + 'static,
{
    _phantom: PhantomData<D>,
    projections: Arc<RwLock<Vec<Box<dyn EventObserver<D>>>>>,
    event_tx: tokio::sync::mpsc::UnboundedSender<Arc<Event<D>>>,
    // Optional: keep handle to background task for shutdown
    _listener_handle: Arc<tokio::task::JoinHandle<()>>,
}
```

#### 2. Update `new()` method

**Current**:
```rust
pub fn new() -> Self {
    log::debug!("Creating a new InMemoryEventBus");
    let projections: Arc<RwLock<Vec<Box<dyn EventObserver<D>>>>> =
        Arc::new(RwLock::new(vec![]));
    Self {
        _phantom: PhantomData,
        projections,
    }
}
```

**New**:
```rust
pub fn new() -> Self {
    log::debug!("Creating a new InMemoryEventBus");
    let projections: Arc<RwLock<Vec<Box<dyn EventObserver<D>>>>> =
        Arc::new(RwLock::new(vec![]));
    
    // Create channel for event notifications
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    
    // Spawn background task to process events (like PgEventBus listener)
    let projections_clone = projections.clone();
    let listener_handle = tokio::spawn(async move {
        log::debug!("InMemoryEventBus: Starting background event processor");
        
        while let Some(event) = event_rx.recv().await {
            log::debug!("InMemoryEventBus: Processing event {}", event.id);
            
            // Process observers sequentially (maintaining order)
            let observers = projections_clone.read().await;
            for observer in observers.iter() {
                if let Err(e) = observer.on_event(Arc::clone(&event)).await {
                    log::error!("Error applying event {}: {:?}", event.id, e);
                    // TODO: Consider dead letter queue like PgEventBus
                }
            }
        }
        
        log::debug!("InMemoryEventBus: Background event processor stopped");
    });
    
    Self {
        _phantom: PhantomData,
        projections,
        event_tx,
        _listener_handle: Arc::new(listener_handle),
    }
}
```

#### 3. Update `publish()` method

**Current**:
```rust
fn publish<'a>(
    &'a self,
    event: Arc<Event<Self::EventType>>,
) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
    Box::pin(async move {
        log::debug!("Publishing event with id: {}", event.id);
        let projections = self.projections.read().await;
        for projection in projections.iter() {
            projection
                .on_event(Arc::clone(&event))
                .await
                .unwrap_or_else(|e| {
                    log::error!("Error applying event: {:?}", e);
                });
        }
        Ok(())
    })
}
```

**New**:
```rust
fn publish<'a>(
    &'a self,
    event: Arc<Event<Self::EventType>>,
) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
    Box::pin(async move {
        log::debug!("Publishing event with id: {}", event.id);
        
        // Send to channel and return immediately (like PgEventBus NO-OP + NOTIFY)
        self.event_tx.send(event).map_err(|e| {
            InMemoryEventBusError::PublishError(e)
        })?;
        
        Ok(())
    })
}
```

#### 4. Update error types

Since we're using `mpsc::UnboundedSender`, we already have the right error type:

```rust
#[derive(Debug, thiserror::Error)]
pub enum InMemoryEventBusError<D>
where
    D: EventData + std::fmt::Debug,
{
    /// The event could not be published
    #[error("Error publishing event: {0}")]
    PublishError(#[from] tokio::sync::mpsc::error::SendError<Arc<Event<D>>>),
    
    // Note: Remove PublishTimeoutError as we're using unbounded channel
}
```

### File: `epoch/examples/saga-order-fulfillment.rs`

#### 5. Simplify saga command dispatching

**Current** (with manual spawning):
```rust
let inventory_agg = self.inventory_aggregate.clone();
tokio::spawn(async move {
    if let Err(e) = inventory_agg
        .handle(Command::new(
            inventory_id,
            ApplicationCommand::ReserveItems {
                order_id,
                items: items_clone,
            },
            None,
            None,
        ))
        .await
    {
        log::error!("Failed to reserve inventory: {}", e);
    }
});
```

**New** (straightforward):
```rust
self.inventory_aggregate
    .handle(Command::new(
        inventory_id,
        ApplicationCommand::ReserveItems {
            order_id: event.stream_id,
            items: items.clone(),
        },
        None,
        None,
    ))
    .await
    .map_err(|e| OrderFulfillmentError::AggregateError(e.to_string()))?;
```

Apply this simplification to all three command dispatches in the saga.

## Implementation Notes

### Channel Choice: Unbounded vs Bounded

**Using `UnboundedSender`**:
- Pro: Matches PgEventBus behavior (NOTIFY doesn't block)
- Pro: Simple - no backpressure concerns
- Pro: `publish()` never blocks
- Con: Could queue unlimited events under extreme load

**Alternative: Bounded channel**:
- Would require handling `SendTimeoutError`
- Adds complexity for questionable benefit in testing scenario
- PgEventBus doesn't have bounded queue semantics

**Decision**: Use `UnboundedSender` to match PgEventBus behavior.

### Background Task Lifecycle

The background task runs indefinitely. When the `InMemoryEventBus` is dropped:
- The sender (`event_tx`) is dropped
- The receiver in the background task gets `None` from `recv()`
- The task exits cleanly

No explicit shutdown needed for simple use cases.

### Sequential Processing

The background task processes observers sequentially (like current behavior):
- Maintains event ordering
- Simpler error handling
- Matches PgEventBus (which also processes sequentially per subscriber)

We're NOT spawning concurrent tasks for each observer - just breaking the synchronous call chain.

## Testing Strategy

### 1. Existing Tests Should Pass

All existing `InMemoryEventBus` tests should continue to work:
- Projection tests
- Aggregate tests
- Any integration tests

The behavior is the same, just async.

### 2. Saga Example Should Work Without Manual Spawning

Run `cargo run --example saga-order-fulfillment` and verify:
- No race conditions
- Saga state transitions correctly
- Order is confirmed at the end

### 3. Add New Test: Verify Async Behavior

Create a test that verifies the async nature:
```rust
#[tokio::test]
async fn test_publish_returns_before_processing() {
    let bus = InMemoryEventBus::new();
    let event = create_test_event();
    
    // Publish should return immediately
    let start = Instant::now();
    bus.publish(Arc::new(event)).await.unwrap();
    let publish_duration = start.elapsed();
    
    // Should be very fast (< 1ms)
    assert!(publish_duration < Duration::from_millis(1));
    
    // Give background task time to process
    tokio::time::sleep(Duration::from_millis(10)).await;
    
    // Verify observer was called
    // ...
}
```

## Migration Impact

### Backward Compatibility: YES

- No changes to public API
- No changes to `EventBus` trait
- Existing code works without modification

### Performance Impact

- **Latency**: Slight increase (one channel send/recv hop)
- **Throughput**: Likely improved (no blocking call chains)
- **Memory**: Minimal (channel buffer for in-flight events)

For testing purposes, this is negligible.

## Documentation Updates

### 1. Update module documentation

Add note explaining the async dispatch model:

```rust
/// An implementation of an in-memory event bus
///
/// Uses a channel-based architecture similar to `PgEventBus`:
/// - `publish()` sends events to a channel and returns immediately
/// - A background task processes events and notifies observers
/// - This matches production PostgreSQL NOTIFY/LISTEN behavior
///
/// This design prevents synchronous call chains that can cause
/// race conditions in sagas and projections.
```

### 2. Update saga example comments

Remove comments about manual spawning and explain the cleaner approach.

## Open Questions

None - the design is straightforward.

## Alternatives Considered

### 1. Keep synchronous and fix saga pattern

We could keep the synchronous bus and require sagas to always spawn commands manually.

**Rejected**: Makes testing diverge from production behavior.

### 2. Add a "mode" flag for sync/async

```rust
InMemoryEventBus::new(EventBusMode::Synchronous)
InMemoryEventBus::new(EventBusMode::Asynchronous)
```

**Rejected**: Adds complexity. Async behavior is better for testing production scenarios.

### 3. Use concurrent dispatch (spawn per observer)

**Rejected**: Discussed in spec 0010 - has downsides and doesn't match PgEventBus sequential processing.

## References

- `epoch_mem/src/event_store.rs` - Current InMemoryEventBus implementation
- `epoch_pg/src/event_bus.rs` - PgEventBus with NOTIFY/LISTEN
- `epoch/examples/saga-order-fulfillment.rs` - Saga with manual spawning
- Spec 0010 - Previous discussion on async dispatch

## Success Criteria

1. ✅ `InMemoryEventBus` uses channels and background task
2. ✅ `publish()` returns immediately without blocking
3. ✅ All existing tests pass
4. ✅ Saga example works without manual `tokio::spawn`
5. ✅ No race conditions in saga state transitions
