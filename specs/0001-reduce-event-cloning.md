# Specification: Reduce Event Cloning with Hybrid Arc/Reference Approach

**Spec ID:** 0001  
**Status:** ✅ Complete  
**Created:** 2026-01-20  
**Completed:** 2026-01-24  

## Problem Statement

Events were cloned excessively throughout the codebase:
- Cloned when publishing to event bus
- Cloned for each projection/observer
- Cloned in saga event processing
- Cloned when streaming from slices

This was inefficient for:
- Events with large payloads
- Systems with many projections (N clones per event)
- High-throughput scenarios

## Solution Overview

Implemented a hybrid ownership model using `Arc` for shared ownership and references for read-only access:

| Operation | Ownership | Rationale |
|-----------|-----------|-----------|
| `EventStoreBackend::store_event` | `Event<T>` (owned) | Store is final owner |
| `EventBus::publish` | `Arc<Event<T>>` | Shared across observers |
| `EventObserver::on_event` | `Arc<Event<T>>` | Can dereference or clone Arc |
| `Projection::apply` | `&Event<T>` | Read-only access |
| `Saga::process_event` | `&Event<T>` | Read-only access |
| `Saga::handle_event` | `&Event<T>` | Read-only access |

### Implementation Phases

**Phase 1:** Arc wrapper for EventBus/EventObserver/Projection
- `EventBus::publish` takes `Arc<Event<T>>`
- `EventObserver::on_event` receives `Arc<Event<T>>`
- Projections dereference to pass `&Event<T>` to `apply()`

**Phase 2:** Saga reference passing
- `Saga::process_event` takes `&Event<ED>`
- `Saga::handle_event` takes `&Event<Self::EventType>`
- Introduced `SagaHandler` wrapper (blanket impl not possible due to trait coherence)
- Introduced `ProjectionHandler` wrapper for consistency

**Phase 3:** Borrow-based event conversion
- Generated `TryFrom<&D>` alongside `TryFrom<D>` in `#[subset_enum]` macro
- Added `to_subset_event_ref()` method
- Changed trait bounds to use `for<'a> TryFrom<&'a ED>`

**Phase 4:** Internal reference streaming
- Added `RefEventStream` trait yielding `&'a Event<D>`
- Added `SliceRefEventStream` for zero-copy iteration
- Added `Projection::re_hydrate_from_refs()` for reference-based streams
- Aggregate re-hydration uses `SliceRefEventStream` internally

## Key Design Decisions

### Why Arc for EventBus?

Events cross `tokio::spawn` boundaries and are distributed to multiple observers:
- `Arc::clone()` is O(1) atomic increment
- Deep clone is O(n) where n = event payload size
- With M observers, saves M-1 deep clones per event

### Why References for Sagas/Projections?

Sagas and projections only read events to make decisions:
- No ownership needed
- Prevents unnecessary clones in calling code
- Works seamlessly with Arc deref coercion

### Why Both TryFrom<D> and TryFrom<&D>?

- `TryFrom<D>` for backward compatibility
- `TryFrom<&D>` for zero-copy conversion
- Clone only matched variant's fields, not entire enum

### Why Wrapper Types (SagaHandler/ProjectionHandler)?

Rust doesn't allow multiple blanket implementations of the same trait:
```rust
// Can't have both:
impl<P: Projection> EventObserver for P { }
impl<S: Saga> EventObserver for S { }
```

Wrapper types solve the orphan rule issue while providing clean ergonomics.

## Alternatives Considered

### Full Arc Everywhere
- Every interface uses `Arc<Event<T>>`
- **Rejected:** Unnecessary for `store_event` which is the terminal owner

### References with Lifetimes
- `on_event(&self, event: &Event<ED>)`
- **Rejected:** Complex lifetime management across async boundaries

### Cow (Clone-on-Write)
- `on_event(&self, event: Cow<'_, Event<ED>>)`
- **Rejected:** More complex API, still needs cloning when ownership required

### Blanket EventObserver for Saga/Projection
- **Rejected:** Rust's orphan rules prevent two blanket impls for the same trait

## Breaking Changes

- `EventBus::publish` signature changed to take `Arc<Event<T>>`
- `EventObserver::on_event` signature changed to take `Arc<Event<T>>`
- `Saga::handle_event` signature changed to take `&Event<T>`
- `Projection::EventType` and `Saga::EventType` now require `TryFrom<&ED>`
- Projections must be wrapped in `ProjectionHandler::new()`
- Sagas must implement `EventObserver` and use `SagaHandler::new()`

## Results

- Eliminated N-1 clones per event (where N = number of observers)
- Eliminated clones during event subset conversion
- Eliminated clones during aggregate re-hydration
- Zero-copy iteration for internal event streams
- Maintained clean ergonomic API
