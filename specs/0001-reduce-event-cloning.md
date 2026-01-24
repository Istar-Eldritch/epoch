# Specification: Reduce Event Cloning with Hybrid Arc/Reference Approach

**Spec ID:** 0001  
**Status:** Complete (All Phases)  
**Created:** 2026-01-20  
**Last Updated:** 2026-01-24  

## Implementation Status

| Phase | Description | Status |
|-------|-------------|--------|
| Phase 1 | Arc wrapper for EventBus/EventObserver/Projection | ✅ Completed (2026-01-20) |
| Phase 2 | Saga reference passing and SagaHandler wrapper | ✅ Completed (2026-01-24) |
| Phase 3 | `to_subset_event` borrow-based conversion | ✅ Completed (2026-01-24) |
| Phase 4 | `SliceEventStream` internal reference stream | ✅ Completed (2026-01-24) |

## 1. Problem Statement

Currently, events are cloned multiple times throughout the `epoch` codebase:

1. **In `EventStoreBackend::store_event`** → cloned to pass to `EventBus::publish`
2. **In `EventBus::publish`** → cloned for each projection/observer
3. **In `EventObserver::on_event`** → takes ownership, forcing clones upstream
4. **In `SliceEventStream`** → clones events from a borrowed slice
5. **In `Saga::process_event`** → takes owned event, forcing clones upstream
6. **In `Saga::handle_event`** → takes owned event, forcing clones in `process_event`

This is inefficient, especially for:
- Events with large payloads
- Systems with many projections (each gets a clone)
- Systems with multiple sagas (each must clone the event)
- High-throughput scenarios

### Current Clone Points

| Location | File | Reason |
|----------|------|--------|
| `store_event` | `epoch_pg/src/event_store.rs:233` | Event needed for both DB insert and bus publish |
| `on_event` loop | `epoch_pg/src/event_bus.rs:205` | Each projection needs the event |
| `publish` loop | `epoch_mem/src/event_store.rs:316` | Each projection needs the event |
| `store_event` | `epoch_mem/src/event_store.rs:135,145` | Storage and bus publish |
| `SliceEventStream` | `epoch_core/src/event_store.rs:122` | Yielding owned events from slice |
| `Saga::process_event` | `epoch_core/src/saga.rs:52` | Takes owned event |
| `Saga::handle_event` | `epoch_core/src/saga.rs:42` | Takes owned event |

## 2. Proposed Solution: Hybrid Arc/Reference Approach

Use different ownership semantics based on the operation's nature:

| Operation | Ownership Model | Rationale |
|-----------|-----------------|-----------|
| `EventStoreBackend::store_event` | `Event<T>` (owned) | Store is the final owner, consumes the event |
| `EventBus::publish` | `Arc<Event<T>>` | Distributes to many observers, shared ownership |
| `EventObserver::on_event` | `Arc<Event<T>>` | Receives shared reference, can hold if needed |
| `Projection::apply` | `&Event<T>` (unchanged) | Already uses reference, no change needed |
| `Saga::process_event` | `&Event<T>` | Receives reference, no ownership needed |
| `Saga::handle_event` | `&Event<T>` | Receives reference, no ownership needed |

### Why This Combination?

1. **`store_event` takes ownership**: The event store is the authoritative source. After storing, it wraps in `Arc` for publishing.

2. **`publish` uses `Arc`**: Events cross `tokio::spawn` boundaries and are distributed to multiple observers. `Arc::clone()` is O(1) atomic increment vs O(n) deep clone.

3. **`on_event` receives `Arc`**: Observers can:
   - Dereference for read-only access (zero cost)
   - Clone the `Arc` if they need to retain the event (cheap)
   - Pass `&*event` to `Projection::apply` or `Saga::process_event` (zero cost)

4. **`apply` uses reference**: Already correct—projections only need to read events.

5. **`process_event` and `handle_event` use references**: Sagas only need to read events to make decisions and dispatch commands. Ownership is unnecessary.

---

# PHASE 1: Arc Wrapper for EventBus/EventObserver/Projection

**Status:** ✅ Completed (2026-01-20)

---

## 3. Interface Changes (Phase 1)

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

---

# PHASE 2: Saga Reference Passing and Handler Wrappers

**Status:** ✅ Completed (2026-01-24)

---

## 4. Problem Statement (Phase 2)

Despite Phase 1 improvements, sagas still require unnecessary cloning:

1. **No blanket `EventObserver` impl for `Saga`**: Unlike `Projection`, consumers must manually implement `EventObserver` for each saga, often inefficiently.

2. **`Saga::process_event` takes owned event**: Forces upstream clones even when only a reference is needed.

3. **`Saga::handle_event` takes owned event**: Forces `process_event` to pass ownership even though sagas only read events.

### Current Saga Clone Flow

```
EventObserver::on_event(Arc<Event<ED>>)
         │
         ▼
    (*event).clone()          ← CLONE: Manual impl clones Arc contents
         │
         ▼
Saga::process_event(Event<ED>)
         │
         ▼
    event.to_subset_event()   ← CLONE: Clones event data for conversion
         │
         ▼
Saga::handle_event(Event<Self::EventType>)  ← Owns converted event
```

With 2 sagas, this means 2 full event clones per published event, plus 2 subset conversions.

## 5. Interface Changes (Phase 2)

### 5.1 `epoch_core/src/saga.rs`

#### `Saga::handle_event` signature

```rust
// BEFORE
async fn handle_event(
    &self,
    state: Self::State,
    event: Event<Self::EventType>,
) -> Result<Option<Self::State>, Self::SagaError>;

// AFTER
async fn handle_event(
    &self,
    state: Self::State,
    event: &Event<Self::EventType>,  // Changed: reference instead of owned
) -> Result<Option<Self::State>, Self::SagaError>;
```

#### `Saga::process_event` signature and implementation

```rust
// BEFORE
async fn process_event(
    &self,
    event: Event<ED>,
) -> Result<(), HandleEventError<...>> {
    if let Ok(event) = event.to_subset_event::<Self::EventType>() {
        // ...
        let state = self.handle_event(state, event).await...;
        // ...
    }
    Ok(())
}

// AFTER
async fn process_event(
    &self,
    event: &Event<ED>,  // Changed: reference instead of owned
) -> Result<(), HandleEventError<...>> {
    if let Ok(event) = event.to_subset_event::<Self::EventType>() {
        // ...
        let state = self.handle_event(state, &event).await...;  // Pass reference
        // ...
    }
    Ok(())
}
```

#### Add `SagaHandler` wrapper type

Since Rust doesn't allow multiple blanket implementations of the same trait, and
`Projection` already had a blanket `EventObserver` impl, we use wrapper types for both.

```rust
use std::sync::Arc;

/// Wrapper that provides EventObserver for Saga types
pub struct SagaHandler<S>(pub S);

impl<S> SagaHandler<S> {
    pub fn new(saga: S) -> Self { Self(saga) }
    pub fn inner(&self) -> &S { &self.0 }
    pub fn into_inner(self) -> S { self.0 }
}

#[async_trait]
impl<ED, S> EventObserver<ED> for SagaHandler<S>
where
    ED: EventData + Send + Sync + 'static,
    S: Saga<ED> + Send + Sync,
    <S::EventType as TryFrom<ED>>::Error: Send + Sync,
{
    async fn on_event(
        &self,
        event: Arc<Event<ED>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.0.process_event(&event).await?;  // Dereference Arc, pass reference
        Ok(())
    }
}
```

### 5.2 `epoch_core/src/projection.rs`

#### Replace blanket impl with `ProjectionHandler` wrapper

For consistency with `SagaHandler`, the `Projection` blanket impl was also converted to a wrapper:

```rust
/// Wrapper that provides EventObserver for Projection types
pub struct ProjectionHandler<P>(pub P);

impl<P> ProjectionHandler<P> {
    pub fn new(projection: P) -> Self { Self(projection) }
    pub fn inner(&self) -> &P { &self.0 }
    pub fn into_inner(self) -> P { self.0 }
}

#[async_trait]
impl<ED, P> EventObserver<ED> for ProjectionHandler<P>
where
    ED: EventData + Send + Sync + 'static,
    P: Projection<ED> + Send + Sync,
    <P::EventType as TryFrom<ED>>::Error: Send + Sync,
{
    async fn on_event(
        &self,
        event: Arc<Event<ED>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.0.apply_and_store(&event).await?;
        Ok(())
    }
}
```

### 5.3 Optimized Saga Event Flow (After Phase 2)

```
EventObserver::on_event(Arc<Event<ED>>)  ← Blanket impl
         │
         ▼
    &*event                              ← DEREF: Zero-cost dereference
         │
         ▼
Saga::process_event(&Event<ED>)
         │
         ▼
    event.to_subset_event()              ← CLONE: Still clones for conversion
         │                                  (acceptable, see Future Work)
         ▼
Saga::handle_event(&Event<Self::EventType>)  ← Borrows converted event
```

**Savings:** Eliminates 1 full event clone per saga. With 2 sagas, saves 2 clones per event.

## 6. Files to Modify

### Phase 1 (Completed)

| File | Changes |
|------|---------|
| `epoch_core/src/event_store.rs` | Update `EventBus::publish` and `EventObserver::on_event` signatures |
| `epoch_core/src/projection.rs` | Update `EventObserver` impl, change `apply_and_store` to take reference |
| `epoch_pg/src/event_store.rs` | Wrap event in `Arc` before publishing |
| `epoch_pg/src/event_bus.rs` | Update `publish` signature, use `Arc::clone` in listener loop |
| `epoch_mem/src/event_store.rs` | Update `EventStoreData`, `InMemoryEventStore`, `InMemoryEventBus`, stream types |
| `epoch_pg/tests/*.rs` | Update test code to match new signatures |
| `epoch_mem/src/event_store.rs` (tests) | Update test code to match new signatures |

### Phase 2 (Completed)

| File | Changes |
|------|---------|
| `epoch_core/src/saga.rs` | Change `handle_event` to `&Event`, change `process_event` to `&Event`, add `SagaHandler` wrapper with `EventObserver` impl |
| `epoch_core/src/projection.rs` | Replace blanket `EventObserver` impl with `ProjectionHandler` wrapper |
| `epoch_mem/src/event_store.rs` | Update tests to use `ProjectionHandler` |

## 7. Breaking Changes

This is a **breaking change** for downstream users who:

### Phase 1
1. Implement custom `EventBus` implementations
2. Implement custom `EventObserver` implementations
3. Call `EventBus::publish` directly
4. Rely on `Event<D>` ownership in `on_event`

### Phase 2
5. Implement the `Saga` trait (must update `handle_event` signature)
6. Implement custom `EventObserver` for sagas (remove manual impl, use blanket)
7. Call `Saga::process_event` directly (must pass reference)

### Migration Guide

#### Phase 1: Custom EventObserver

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

#### Phase 2: Saga Implementation

```rust
// Before: Saga with manual EventObserver
#[async_trait]
impl Saga<ApplicationEvent> for MySaga {
    // ...
    async fn handle_event(
        &self,
        state: Self::State,
        event: Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::SagaError> {
        match event.data {
            // ...
        }
    }
}

#[async_trait]
impl EventObserver<ApplicationEvent> for MySaga {
    async fn on_event(
        &self,
        event: Arc<Event<ApplicationEvent>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.process_event((*event).clone()).await?;  // Cloning here!
        Ok(())
    }
}

// Before: Subscribing
event_bus.subscribe(my_saga).await?;

// After: Saga with SagaHandler wrapper
use epoch_core::saga::SagaHandler;

#[async_trait]
impl Saga<ApplicationEvent> for MySaga {
    // ...
    async fn handle_event(
        &self,
        state: Self::State,
        event: &Event<Self::EventType>,  // Now takes reference
    ) -> Result<Option<Self::State>, Self::SagaError> {
        match &event.data {  // Borrow data
            // ...
        }
    }
}
// No manual EventObserver impl needed - SagaHandler provides it!

// After: Subscribing with wrapper
event_bus.subscribe(SagaHandler::new(my_saga)).await?;
```

#### Phase 2: Projection Subscription

```rust
// Before: Direct subscription (blanket impl)
event_bus.subscribe(my_projection).await?;

// After: Wrap in ProjectionHandler
use epoch_core::projection::ProjectionHandler;
event_bus.subscribe(ProjectionHandler::new(my_projection)).await?;
```

## 8. Testing Strategy

1. **Unit Tests:** All existing tests should pass after updating to new signatures
2. **Integration Tests:** Verify events flow correctly through the system
3. **Saga Tests:** Verify sagas receive and process events correctly with new signatures

## 9. Dependencies

No new dependencies required. `std::sync::Arc` is in the standard library.

## 10. Alternatives Considered

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

### E. Make `handle_event` take `Arc<Event>`
- Rejected: Sagas don't need shared ownership, just read access

### F. Blanket `EventObserver` impl for `Saga`
- Attempted but rejected due to Rust's orphan rules
- Two blanket impls for `EventObserver` (one for `Projection`, one for `Saga`) conflict
- The compiler can't prove that a type won't implement both traits
- Solution: Use wrapper types (`ProjectionHandler`, `SagaHandler`) instead

---

# PHASE 3: `to_subset_event` Borrow-Based Conversion

**Status:** ✅ Completed (2026-01-24)

---

## 11. Problem Statement (Phase 3)

Currently `to_subset_event()` clones event data via `TryFrom<D>`:

```rust
pub fn to_subset_event<ED>(&self) -> Result<Event<ED>, ED::Error>
where
    ED: EventData + TryFrom<D>,
{
    let data = self
        .data
        .as_ref()
        .map(|d| ED::try_from(d.clone()))  // ← Clone here
        .transpose()?;
    // ...
}
```

This clone happens in:
- `Projection::re_hydrate` - for each event during projection rebuild
- `Projection::apply_and_store` - for each event during live processing
- `Saga::process_event` - for each event during saga processing

**Impact:** Medium-High. This clone happens on every event processed by every projection and saga.

## 12. Proposed Solution (Phase 3)

### 12.1 Generate `TryFrom<&D>` in `subset_enum` derive macro

The `#[subset_enum]` macro in `epoch_derive` currently generates:

```rust
impl TryFrom<SupersetEnum> for SubsetEnum {
    type Error = ConversionError;
    
    fn try_from(value: SupersetEnum) -> Result<Self, Self::Error> {
        match value {
            SupersetEnum::VariantA { .. } => Ok(SubsetEnum::VariantA { .. }),
            _ => Err(ConversionError::...),
        }
    }
}
```

Add a parallel implementation for references:

```rust
impl TryFrom<&SupersetEnum> for SubsetEnum {
    type Error = ConversionError;
    
    fn try_from(value: &SupersetEnum) -> Result<Self, Self::Error> {
        match value {
            SupersetEnum::VariantA { field1, field2, .. } => Ok(SubsetEnum::VariantA { 
                field1: field1.clone(),  // Clone individual fields, not entire enum
                field2: field2.clone(),
            }),
            _ => Err(ConversionError::...),
        }
    }
}
```

### 12.2 Update `to_subset_event` to use reference conversion

```rust
// Before
pub fn to_subset_event<ED>(&self) -> Result<Event<ED>, ED::Error>
where
    ED: EventData + TryFrom<D>,
{
    let data = self.data.as_ref().map(|d| ED::try_from(d.clone())).transpose()?;
    // ...
}

// After
pub fn to_subset_event<ED>(&self) -> Result<Event<ED>, ED::Error>
where
    ED: EventData + for<'a> TryFrom<&'a D>,  // Changed bound
{
    let data = self.data.as_ref().map(|d| ED::try_from(d)).transpose()?;  // No clone
    // ...
}
```

### 12.3 Benefits

| Scenario | Before (clone entire enum) | After (clone matching fields) |
|----------|---------------------------|------------------------------|
| Event matches subset | Clone all variants' data | Clone only matched variant's fields |
| Event doesn't match | Clone all, then discard | No clone, early return |
| Large non-matching variants | Wasted clone | Zero cost |

### 12.4 Implementation Notes

**Approach Taken:** We updated the trait bounds and internal implementation to use reference-based conversion by default:

1. Added `TryFrom<&D>` generation in the `subset_enum` macro alongside existing `TryFrom<D>`
2. Added a new `to_subset_event_ref()` method that uses the reference-based conversion
3. Changed `Projection::EventType` and `Saga::EventType` bounds to require `TryFrom<&ED, Error = EnumConversionError>`
4. Updated internal `re_hydrate`, `apply_and_store`, and `process_event` to use `to_subset_event_ref()`

### 12.5 Files Modified

| File | Changes |
|------|---------|
| `epoch_derive/src/subset.rs` | Generate `TryFrom<&D>` impl alongside `TryFrom<D>` in `subset_enum` macro |
| `epoch_core/src/event.rs` | Add `to_subset_event_ref()` method with reference-based conversion |
| `epoch_core/src/projection.rs` | Change `EventType` bound to `TryFrom<&ED, Error = EnumConversionError>`, use `to_subset_event_ref()` internally |
| `epoch_core/src/saga.rs` | Change `EventType` bound to `TryFrom<&ED, Error = EnumConversionError>`, use `to_subset_event_ref()` internally |
| `epoch_core/src/aggregate.rs` | Update `ReHydrateError` type parameter to use `EnumConversionError` |

### 12.6 Breaking Changes

This is a **breaking change**:
- `Projection::EventType` now requires `for<'a> TryFrom<&'a ED, Error = EnumConversionError>` instead of `TryFrom<ED>`
- `Saga::EventType` now requires `for<'a> TryFrom<&'a ED, Error = EnumConversionError>` instead of `TryFrom<ED>`
- For subset enums using `#[subset_enum]`, this is automatic
- For identity conversions, users must add a manual `TryFrom<&Self>` impl

---

# PHASE 4: `SliceEventStream` Internal Reference Stream

**Status:** ✅ Completed (2026-01-24)

---

## 13. Problem Statement (Phase 4)

`SliceEventStream` clones events because `EventStream` trait yields owned `Event<D>`:

```rust
impl Stream for SliceEventStream<'a, D, E> {
    type Item = Result<Event<D>, E>;  // Owned
    
    fn poll_next(...) {
        let event = this.inner[this.idx].clone();  // Must clone
        Poll::Ready(Some(Ok(event)))
    }
}
```

## 14. Analysis of Options (Phase 4)

| Option | Description | Pros | Cons |
|--------|-------------|------|------|
| A. Yield `&'a Event<D>` | Change trait to return references | Zero-cost | Lifetime contagion through entire API; `PgEventStream` can't return refs (deserializes on-the-fly); `InMemoryEventStoreStream` holds mutex lock |
| B. Yield `Arc<Event<D>>` | Change trait to return Arc | Consistent with Phase 1 | `PgEventStream` gains Arc overhead; `SliceEventStream` needs Arc input or wraps on-the-fly |
| C. Internal reference stream | Keep `EventStream` as-is; create separate internal type for `SliceEventStream` use case | Minimal API change; targeted fix | Adds complexity; two patterns for streaming |

## 15. Proposed Solution (Phase 4)

**Decision:** Option C (internal reference stream).

Create a separate `RefEventStream` trait and `SliceRefEventStream` for internal use in `Aggregate::handle`:

```rust
/// Internal trait for reference-based event streaming (not public API)
trait RefEventStream<'a, D, E>: Stream<Item = Result<&'a Event<D>, E>> + Send
where
    D: EventData + Send + Sync,
{
}

/// Reference-based slice stream for aggregate re-hydration
struct SliceRefEventStream<'a, D, E> {
    inner: &'a [Event<D>],
    idx: usize,
    _marker: PhantomData<E>,
}

impl<'a, D, E> Stream for SliceRefEventStream<'a, D, E> {
    type Item = Result<&'a Event<D>, E>;
    
    fn poll_next(...) {
        if this.idx < this.inner.len() {
            let event = &this.inner[this.idx];  // No clone!
            this.idx += 1;
            Poll::Ready(Some(Ok(event)))
        } else {
            Poll::Ready(None)
        }
    }
}
```

Add a separate `re_hydrate_from_refs` method or make `re_hydrate` generic over stream item type.

## 16. Implementation Notes (Phase 4)

**Approach Taken:** Option C (internal reference stream) as proposed.

### 16.1 New Types Added

**`epoch_core/src/event_store.rs`:**

```rust
/// Trait for reference-based event streaming
pub trait RefEventStream<'a, D, E>: Stream<Item = Result<&'a Event<D>, E>> + Send
where
    D: EventData + Send + Sync + 'a,
{
}

/// Reference-based slice stream for aggregate re-hydration  
pub struct SliceRefEventStream<'a, D, E> {
    inner: &'a [Event<D>],
    idx: usize,
    _marker: PhantomData<E>,
}
```

**`epoch_core/src/projection.rs`:**

```rust
/// Reconstructs the projection's state from a reference-based event stream.
async fn re_hydrate_from_refs<'a, E>(
    &self,
    state: Option<Self::State>,
    event_stream: Pin<Box<dyn RefEventStream<'a, ED, E> + Send + 'a>>,
) -> Result<Option<Self::State>, ReHydrateError<...>>
```

### 16.2 Aggregate::handle Updated

**`epoch_core/src/aggregate.rs`:**

```rust
// Before
let event_stream = Box::pin(SliceEventStream::from(events.as_slice()));
let state = self.re_hydrate(state, event_stream).await...;

// After  
let event_stream = Box::pin(SliceRefEventStream::from(events.as_slice()));
let state = self.re_hydrate_from_refs(state, event_stream).await...;
```

### 16.3 Files Modified

| File | Changes |
|------|---------|
| `epoch_core/src/event_store.rs` | Add `RefEventStream` trait and `SliceRefEventStream` struct |
| `epoch_core/src/projection.rs` | Add `re_hydrate_from_refs()` method |
| `epoch_core/src/aggregate.rs` | Update `handle()` to use `SliceRefEventStream` and `re_hydrate_from_refs()` |
| `epoch_core/tests/slice_ref_event_stream_tests.rs` | New test file with 6 tests |

### 16.4 Benefits

- **Zero-copy iteration**: Events are borrowed from the slice, no cloning required
- **Same semantics**: `re_hydrate_from_refs` produces identical results to `re_hydrate`
- **Minimal API change**: Public `EventStream` trait unchanged, new types are additive
- **Backwards compatible**: Existing code using `SliceEventStream` still works

## 17. Open Questions

1. ~~Should `EventStream` also yield `Arc<Event<D>>`?~~ → **Resolved:** No. Analysis showed that changing `EventStream` to yield `Arc<Event<D>>` would add overhead to `PgEventStream` (which deserializes fresh events) while only benefiting `InMemoryEventStoreStream` and `SliceEventStream`. The `EventBus::publish` path (Phase 1) benefits from `Arc` because events fan out to multiple observers. The `EventStream` path is typically single-consumer, so `Arc` overhead isn't justified.

2. ~~Should we provide a helper method `Event::into_arc(self) -> Arc<Event<D>>`?~~ → **Resolved:** Not necessary, `Arc::new(event)` is clear enough.

3. ~~Should `process_event` take `&Event<ED>` or `Arc<Event<ED>>`?~~ → **Resolved:** Take `&Event<ED>`. The blanket impl dereferences the `Arc`.

4. ~~Should the `Saga` blanket impl be feature-gated?~~ → **Resolved:** Not applicable. We use wrapper types (`SagaHandler`, `ProjectionHandler`) instead of blanket impls to avoid trait coherence issues.

5. ~~Should `SliceEventStream` be optimized to avoid cloning?~~ → **Resolved:** Deferred to Phase 4. The clone only occurs on the write path (1-3 events per command, freshly created). Option C (internal reference stream) will be implemented if profiling shows it's a bottleneck.

6. ~~Should `TryFrom<&D>` be generated alongside or instead of `TryFrom<D>`?~~ → **Resolved:** Both are generated. The `#[subset_enum]` macro generates `TryFrom<D>` for backwards compatibility and `TryFrom<&D>` for efficient reference-based conversion.

## 18. Approval

### Phase 1
- [x] Reviewed by project maintainer
- [x] Implementation plan approved
- [x] Implementation completed

### Phase 2
- [x] Reviewed by project maintainer
- [x] Implementation plan approved
- [x] Implementation completed

### Phase 3
- [x] Reviewed by project maintainer
- [x] Implementation plan approved
- [x] Implementation completed (2026-01-24)

### Phase 4
- [x] Reviewed by project maintainer
- [x] Implementation plan approved
- [x] Implementation completed (2026-01-24)
