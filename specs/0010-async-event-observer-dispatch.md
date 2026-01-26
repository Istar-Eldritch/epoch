# Specification: Asynchronous Event Observer Dispatch

## Status: Proposed

## Overview

Move the asynchronous spawning of event observer handlers from user code (like sagas) into the event bus implementation itself. This will prevent event observers from blocking each other and eliminate the need for manual `tokio::spawn` in saga implementations.

## Motivation

### Current Problem

Currently, when an event is published to the event bus, observers are notified sequentially:

```rust
// InMemoryEventBus::publish (epoch_mem/src/event_store.rs)
for projection in projections.iter() {
    projection.on_event(Arc::clone(&event)).await.unwrap_or_else(|e| {
        log::error!("Error applying event: {:?}", e);
    });
}
```

This creates several issues:

1. **Observer Blocking**: Each observer blocks the next one. If a saga dispatches commands synchronously in its `handle_event`, those commands publish new events, which then trigger other observers, all before the first saga's state is persisted.

2. **Race Conditions**: In the saga example, we had to manually spawn background tasks to dispatch commands:
   ```rust
   tokio::spawn(async move {
       inventory_agg.handle(command).await?;
   });
   ```
   Without this workaround, the saga receives subsequent events before its state from the previous event is persisted, causing unexpected state transitions.

3. **Performance**: Slow observers block fast ones, limiting throughput and scalability.

4. **User Burden**: Users must understand these internals and work around them manually.

### Expected Behavior

Event observers should be notified **asynchronously and concurrently** so that:
- Each observer can process events independently without blocking others
- Saga state is persisted before the next event arrives (natural ordering)
- Users can write straightforward saga code without manual spawning
- Better performance through concurrent processing

## Goals

1. **Concurrent Observer Notification**: Spawn each observer's `on_event` call in a separate task
2. **Backward Compatibility**: Existing code should continue to work without changes
3. **Error Handling**: Maintain existing error handling and logging behavior
4. **PostgreSQL Parity**: Apply the same pattern to `PgEventBus` if appropriate

## Non-Goals

- Guaranteed ordering of event processing across observers (already not guaranteed)
- Synchronous event bus option (can be added later if needed)
- Changing the `EventObserver` trait signature

## Proposed Changes

### 1. Update `InMemoryEventBus::publish`

**File**: `epoch_mem/src/event_store.rs`

**Current Implementation** (lines ~394-410):
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

**New Implementation**:
```rust
fn publish<'a>(
    &'a self,
    event: Arc<Event<Self::EventType>>,
) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
    Box::pin(async move {
        log::debug!("Publishing event with id: {}", event.id);
        let projections = self.projections.read().await;
        
        // Spawn each observer in a separate task for concurrent processing
        let mut handles = Vec::with_capacity(projections.len());
        
        for projection in projections.iter() {
            let event = Arc::clone(&event);
            // Clone the Arc<dyn EventObserver> - this is cheap
            let observer = projection.clone();
            
            let handle = tokio::spawn(async move {
                if let Err(e) = observer.on_event(event).await {
                    log::error!("Error applying event: {:?}", e);
                }
            });
            
            handles.push(handle);
        }
        
        // Optionally: Don't wait for all observers to complete
        // This makes publish() non-blocking, but observers still process asynchronously
        // drop(handles);
        
        // Or: Wait for all observers to complete (maintains current behavior of waiting)
        for handle in handles {
            let _ = handle.await; // Ignore JoinErrors
        }
        
        Ok(())
    })
}
```

**Challenge**: `Box<dyn EventObserver<D>>` is not `Clone`. We need to either:

**Option A**: Store `Arc<dyn EventObserver<D>>` instead of `Box<dyn EventObserver<D>>`
- Change: `Arc<RwLock<Vec<Arc<dyn EventObserver<D>>>>>`
- Pro: Simple, enables cloning for concurrent dispatch
- Con: Requires changing how observers are stored

**Option B**: Don't wait for observers (fire-and-forget)
- Drop handles immediately after spawning
- Pro: Maximum concurrency, simpler code
- Con: Less control over completion, potential unbounded task spawning

**Option C**: Use channels for observer notification
- Each observer gets its own channel
- Separate long-running task processes events from channel
- Pro: Better control, natural backpressure
- Con: More complex implementation

### 2. Make EventObserver Clone or use Arc

**Recommended Approach**: Store observers as `Arc<Mutex<dyn EventObserver<D>>>` or `Arc<dyn EventObserver<D> + Send + Sync>`.

Since `EventObserver::on_event` takes `&self`, we can use `Arc` directly:

**File**: `epoch_mem/src/event_store.rs`

**Change storage type**:
```rust
pub struct InMemoryEventBus<D>
where
    D: EventData + Send + Sync + 'static,
{
    _phantom: PhantomData<D>,
    // Changed from Vec<Box<...>> to Vec<Arc<...>>
    projections: Arc<RwLock<Vec<Arc<dyn EventObserver<D>>>>>,
}
```

**Update subscribe method**:
```rust
fn subscribe<T>(
    &self,
    projector: T,
) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send>>
where
    T: EventObserver<Self::EventType> + Send + Sync + 'static,
{
    log::debug!("Subscribing projector to InMemoryEventBus");
    let projectors = self.projections.clone();
    Box::pin(async move {
        let mut projectors = projectors.write().await;
        // Wrap in Arc instead of Box
        projectors.push(Arc::new(projector));
        Ok(())
    })
}
```

### 3. Update saga example to remove manual spawning

**File**: `epoch/examples/saga-order-fulfillment.rs`

**Simplify command dispatching**:
```rust
// Before (manual spawning):
let inventory_agg = self.inventory_aggregate.clone();
tokio::spawn(async move {
    if let Err(e) = inventory_agg.handle(command).await {
        log::error!("Failed to reserve inventory: {}", e);
    }
});

// After (straightforward):
self.inventory_aggregate.handle(command).await
    .map_err(|e| OrderFulfillmentError::AggregateError(e.to_string()))?;
```

This makes saga code much cleaner and more intuitive.

### 4. Consider PgEventBus

The PostgreSQL event bus has more complex retry and DLQ logic. Review whether concurrent dispatch is appropriate there, or if sequential processing with locks is intentional for:
- Checkpoint consistency
- Retry ordering
- DLQ behavior

**Recommendation**: Start with `InMemoryEventBus` for testing/examples, then evaluate `PgEventBus` separately.

## Implementation Strategy

### Phase 1: InMemoryEventBus (Proof of Concept)
1. Change storage from `Box` to `Arc` for observers
2. Implement concurrent dispatch with `tokio::spawn`
3. Update saga example to remove manual spawning
4. Test for race conditions and ordering issues

### Phase 2: Evaluation
1. Run existing tests to ensure backward compatibility
2. Benchmark performance improvements
3. Test saga example behavior

### Phase 3: PgEventBus (If Appropriate)
1. Analyze impact on checkpointing and DLQ
2. Implement concurrent dispatch if compatible with guarantees
3. Update documentation

## Testing Strategy

1. **Saga Example**: Verify the saga works correctly without manual spawning
2. **Existing Tests**: Run all existing projection and saga tests
3. **Concurrency Tests**: Create tests that verify observers don't block each other
4. **Race Condition Tests**: Ensure saga state is persisted before receiving next event

## Migration Path

This change is **backward compatible** for users:
- Existing projections and sagas work as-is
- No API changes to `EventObserver` trait
- Only internal implementation changes

## Alternatives Considered

### 1. Keep Sequential Processing
- Pro: Simpler, predictable ordering
- Con: Doesn't solve the saga race condition problem

### 2. Make Concurrency Optional (Configuration Flag)
- Pro: Users can choose sequential vs concurrent
- Con: More complexity, two code paths to maintain

### 3. Event Bus Channels
- Pro: Better backpressure control, more robust
- Con: Significant implementation complexity

## Open Questions

1. **Wait for completion or fire-and-forget?**
   - Waiting ensures all observers process before `publish()` returns
   - Fire-and-forget provides maximum concurrency but less control

2. **PgEventBus compatibility?**
   - Does concurrent dispatch break checkpoint/DLQ guarantees?
   - Should PgEventBus remain sequential for reliability?

3. **Error aggregation?**
   - Currently errors are just logged
   - Should we collect and return all errors from observers?

4. **Bounded concurrency?**
   - Should we limit concurrent observer tasks?
   - Risk of unbounded task spawning with many events/observers?

## References

- `epoch_mem/src/event_store.rs` - InMemoryEventBus implementation
- `epoch_pg/src/event_bus.rs` - PgEventBus implementation  
- `epoch/examples/saga-order-fulfillment.rs` - Saga with manual spawning
- `epoch_core/src/saga.rs` - Saga trait definition
- `epoch_core/src/projection.rs` - Projection trait definition

## Related Issues

This change addresses the fundamental issue that caused us to add manual `tokio::spawn` in the saga example. It's a framework-level improvement that benefits all users.
