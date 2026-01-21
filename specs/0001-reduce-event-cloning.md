# Specification: Reduce Event Cloning with Hybrid Arc/Reference Approach

**Spec ID:** 0001  
**Status:** Implemented  
**Created:** 2026-01-20  
**Implemented:** 2026-01-20  

## 1. Problem Statement

Currently, events are cloned multiple times throughout the `epoch` codebase:

1. **In `EventStoreBackend::store_event`** → cloned to pass to `EventBus::publish`
2. **In `EventBus::publish`** → cloned for each projection/observer
3. **In `EventObserver::on_event`** → takes ownership, forcing clones upstream
4. **In `SliceEventStream`** → clones events from a borrowed slice

This is inefficient, especially for:
- Events with large payloads
- Systems with many projections (each gets a clone)
- High-throughput scenarios

### Current Clone Points

| Location | File | Reason |
|----------|------|--------|
| `store_event` | `epoch_pg/src/event_store.rs:233` | Event needed for both DB insert and bus publish |
| `on_event` loop | `epoch_pg/src/event_bus.rs:205` | Each projection needs the event |
| `publish` loop | `epoch_mem/src/event_store.rs:316` | Each projection needs the event |
| `store_event` | `epoch_mem/src/event_store.rs:135,145` | Storage and bus publish |
| `SliceEventStream` | `epoch_core/src/event_store.rs:122` | Yielding owned events from slice |

## 2. Proposed Solution: Hybrid Arc/Reference Approach

Use different ownership semantics based on the operation's nature:

| Operation | Ownership Model | Rationale |
|-----------|-----------------|-----------|
| `EventStoreBackend::store_event` | `Event<T>` (owned) | Store is the final owner, consumes the event |
| `EventBus::publish` | `Arc<Event<T>>` | Distributes to many observers, shared ownership |
| `EventObserver::on_event` | `Arc<Event<T>>` | Receives shared reference, can hold if needed |
| `Projection::apply` | `&Event<T>` (unchanged) | Already uses reference, no change needed |

### Why This Combination?

1. **`store_event` takes ownership**: The event store is the authoritative source. After storing, it wraps in `Arc` for publishing.

2. **`publish` uses `Arc`**: Events cross `tokio::spawn` boundaries and are distributed to multiple observers. `Arc::clone()` is O(1) atomic increment vs O(n) deep clone.

3. **`on_event` receives `Arc`**: Observers can:
   - Dereference for read-only access (zero cost)
   - Clone the `Arc` if they need to retain the event (cheap)
   - Pass `&*event` to `Projection::apply` (zero cost)

4. **`apply` uses reference**: Already correct—projections only need to read events.

## 3. Interface Changes

### 3.1 `epoch_core/src/event_store.rs`

#### `EventBus` trait

```rust
// BEFORE
pub trait EventBus {
    type EventType: EventData + Send + Sync + 'static;
    type Error: std::error::Error;
    
    fn publish<'a>(
        &'a self,
        event: Event<Self::EventType>,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>>;

    fn subscribe<T>(
        &self,
        projector: T,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send>>
    where
        T: EventObserver<Self::EventType> + Send + Sync + 'static;
}

// AFTER
use std::sync::Arc;

pub trait EventBus {
    type EventType: EventData + Send + Sync + 'static;
    type Error: std::error::Error;
    
    fn publish<'a>(
        &'a self,
        event: Arc<Event<Self::EventType>>,  // Changed: Arc wrapper
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>>;

    fn subscribe<T>(
        &self,
        projector: T,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send>>
    where
        T: EventObserver<Self::EventType> + Send + Sync + 'static;
}
```

#### `EventObserver` trait

```rust
// BEFORE
#[async_trait]
pub trait EventObserver<ED>: Send + Sync
where
    ED: EventData + Send + Sync,
{
    async fn on_event(
        &self,
        event: Event<ED>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

// AFTER
use std::sync::Arc;

#[async_trait]
pub trait EventObserver<ED>: Send + Sync
where
    ED: EventData + Send + Sync,
{
    async fn on_event(
        &self,
        event: Arc<Event<ED>>,  // Changed: Arc wrapper
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}
```

### 3.2 `epoch_core/src/projection.rs`

#### `EventObserver` implementation for `Projection`

```rust
// BEFORE
#[async_trait]
impl<ED, T> EventObserver<ED> for T
where
    ED: EventData + Send + Sync + 'static,
    T: Projection<ED> + Send + Sync,
{
    async fn on_event(
        &self,
        event: Event<ED>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.apply_and_store(event).await?;
        Ok(())
    }
}

// AFTER
use std::sync::Arc;

#[async_trait]
impl<ED, T> EventObserver<ED> for T
where
    ED: EventData + Send + Sync + 'static,
    T: Projection<ED> + Send + Sync,
{
    async fn on_event(
        &self,
        event: Arc<Event<ED>>,  // Changed: Arc wrapper
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.apply_and_store(&event).await?;  // Pass reference
        Ok(())
    }
}
```

#### `Projection::apply_and_store` signature

```rust
// BEFORE
async fn apply_and_store(
    &self,
    event: Event<ED>,
) -> Result<...>

// AFTER
async fn apply_and_store(
    &self,
    event: &Event<ED>,  // Changed: reference instead of owned
) -> Result<...>
```

### 3.3 `epoch_pg/src/event_store.rs`

#### `PgEventStore::store_event`

```rust
// BEFORE
async fn store_event(&self, event: Event<Self::EventType>) -> Result<(), Self::Error> {
    // ... SQL insert ...
    
    self.bus
        .publish(event.clone())  // Clone here
        .await
        .map_err(|e| PgEventStoreError::BUSPublishError(e))?;

    Ok(())
}

// AFTER
async fn store_event(&self, event: Event<Self::EventType>) -> Result<(), Self::Error> {
    // ... SQL insert (uses &event for bindings) ...
    
    self.bus
        .publish(Arc::new(event))  // Wrap in Arc, no clone
        .await
        .map_err(|e| PgEventStoreError::BUSPublishError(e))?;

    Ok(())
}
```

### 3.4 `epoch_pg/src/event_bus.rs`

#### `PgEventBus` listener loop

```rust
// BEFORE
let event = Event::<D> { ... };

let mut projections_guard = projections.lock().await;
for projection in projections_guard.iter_mut() {
    let projection_guard = projection.lock().await;
    match projection_guard.on_event(event.clone()).await {  // Clone per projection
        // ...
    }
}

// AFTER
let event = Arc::new(Event::<D> { ... });  // Wrap once

let mut projections_guard = projections.lock().await;
for projection in projections_guard.iter_mut() {
    let projection_guard = projection.lock().await;
    match projection_guard.on_event(Arc::clone(&event)).await {  // Cheap Arc clone
        // ...
    }
}
```

#### `PgEventBus::publish` (currently no-op, but signature must change)

```rust
// BEFORE
fn publish<'a>(
    &'a self,
    _event: Event<Self::EventType>,
) -> Pin<Box<dyn std::future::Future<Output = Result<(), Self::Error>> + Send + 'a>> {
    Box::pin(async { Ok(()) })
}

// AFTER
fn publish<'a>(
    &'a self,
    _event: Arc<Event<Self::EventType>>,  // Changed signature
) -> Pin<Box<dyn std::future::Future<Output = Result<(), Self::Error>> + Send + 'a>> {
    Box::pin(async { Ok(()) })
}
```

### 3.5 `epoch_mem/src/event_store.rs`

#### `InMemoryEventStore::store_event`

```rust
// BEFORE
async fn store_event(&self, event: Event<Self::EventType>) -> Result<(), Self::Error> {
    // ...
    data.events.insert(event_id, event.clone());
    // ...
    if bus.publish(event.clone()).await.is_err() {
        return Err(InMemoryEventStoreBackendError::PublishEvent);
    }
    Ok(())
}

// AFTER
async fn store_event(&self, event: Event<Self::EventType>) -> Result<(), Self::Error> {
    // ...
    let event = Arc::new(event);  // Wrap once
    data.events.insert(event_id, Arc::clone(&event));  // Store Arc
    // ...
    if bus.publish(event).await.is_err() {  // Move Arc to publish
        return Err(InMemoryEventStoreBackendError::PublishEvent);
    }
    Ok(())
}
```

**Note:** This requires changing `EventStoreData` to store `Arc<Event<D>>`:

```rust
// BEFORE
struct EventStoreData<D: EventData> {
    events: HashMap<Uuid, Event<D>>,
    // ...
}

// AFTER
struct EventStoreData<D: EventData> {
    events: HashMap<Uuid, Arc<Event<D>>>,
    // ...
}
```

#### `InMemoryEventBus::publish`

```rust
// BEFORE
fn publish<'a>(
    &'a self,
    event: Event<Self::EventType>,
) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
    Box::pin(async move {
        let projections = self.projections.read().await;
        for projection in projections.iter() {
            projection.on_event(event.clone()).await...  // Clone per projection
        }
        Ok(())
    })
}

// AFTER
fn publish<'a>(
    &'a self,
    event: Arc<Event<Self::EventType>>,
) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
    Box::pin(async move {
        let projections = self.projections.read().await;
        for projection in projections.iter() {
            projection.on_event(Arc::clone(&event)).await...  // Cheap Arc clone
        }
        Ok(())
    })
}
```

#### `InMemoryEventStoreStream::poll_next`

```rust
// BEFORE
if let Some(event) = data.events.get(&event_id) {
    return Poll::Ready(Some(Ok(event.clone())));  // Deep clone
}

// AFTER
if let Some(event) = data.events.get(&event_id) {
    return Poll::Ready(Some(Ok(Arc::clone(event))));  // Cheap Arc clone
}
```

**Note:** This requires changing the stream's `Item` type:

```rust
// Stream yields Arc<Event<...>> instead of Event<...>
type Item = Result<Arc<Event<B::EventType>>, E>;
```

### 3.6 `epoch_core/src/event_store.rs` - `SliceEventStream`

This is used in `Aggregate::handle` to replay events. Two options:

**Option A: Change to yield `Arc<Event<D>>`**
```rust
// Requires storing Arc<Event<D>> in the slice, which is awkward
```

**Option B: Keep as-is for slice (accept clone for this use case)**

The `SliceEventStream` is used internally in `Aggregate::handle` with a small, local slice of newly created events. The clone cost here is acceptable since:
- It's a small number of events (typically 1-3)
- Events were just created, so they're hot in cache
- Changing this would require larger refactoring

**Recommendation:** Keep `SliceEventStream` as-is for now. Mark with a `// TODO` for future optimization if profiling shows it's a bottleneck.

## 4. Files to Modify

| File | Changes |
|------|---------|
| `epoch_core/src/event_store.rs` | Update `EventBus::publish` and `EventObserver::on_event` signatures |
| `epoch_core/src/projection.rs` | Update `EventObserver` impl, change `apply_and_store` to take reference |
| `epoch_pg/src/event_store.rs` | Wrap event in `Arc` before publishing |
| `epoch_pg/src/event_bus.rs` | Update `publish` signature, use `Arc::clone` in listener loop |
| `epoch_mem/src/event_store.rs` | Update `EventStoreData`, `InMemoryEventStore`, `InMemoryEventBus`, stream types |
| `epoch_pg/tests/*.rs` | Update test code to match new signatures |
| `epoch_mem/src/event_store.rs` (tests) | Update test code to match new signatures |

## 5. Breaking Changes

This is a **breaking change** for downstream users who:

1. Implement custom `EventBus` implementations
2. Implement custom `EventObserver` implementations
3. Call `EventBus::publish` directly
4. Rely on `Event<D>` ownership in `on_event`

### Migration Guide

```rust
// Before: Custom EventObserver
#[async_trait]
impl EventObserver<MyEvent> for MyObserver {
    async fn on_event(&self, event: Event<MyEvent>) -> Result<(), ...> {
        // use event directly
    }
}

// After: Custom EventObserver
use std::sync::Arc;

#[async_trait]
impl EventObserver<MyEvent> for MyObserver {
    async fn on_event(&self, event: Arc<Event<MyEvent>>) -> Result<(), ...> {
        // use &*event or event.field_name (auto-deref)
        // if ownership needed: (*event).clone() or Arc::clone(&event)
    }
}
```

## 6. Testing Strategy

1. **Unit Tests:** All existing tests should pass after updating to new signatures
2. **Integration Tests:** Verify events flow correctly through the system

## 7. Dependencies

No new dependencies required. `std::sync::Arc` is in the standard library.

## 8. Alternatives Considered

### A. Full Arc everywhere
- Every interface uses `Arc<Event<T>>`
- Rejected: Unnecessary for `store_event` which is the terminal owner

### B. References with lifetimes
- `on_event(&self, event: &Event<ED>)`
- Rejected: Complex lifetime management across async boundaries and `tokio::spawn`

### C. Cow (Clone-on-Write)
- `on_event(&self, event: Cow<'_, Event<ED>>)`
- Rejected: More complex API, still needs cloning when ownership required

### D. Keep current design
- Rejected: Inefficient for high-throughput systems with many projections

## 9. Open Questions

1. Should `EventStream` also yield `Arc<Event<D>>`? This would affect `read_events` and `read_events_since`.
   - **Recommendation:** Yes, for consistency and to avoid clones when replaying events.

2. Should we provide a helper method `Event::into_arc(self) -> Arc<Event<D>>`?
   - **Recommendation:** Not necessary, `Arc::new(event)` is clear enough.

## 10. Approval

- [x] Reviewed by project maintainer
- [x] Implementation plan approved
- [x] Implementation completed
