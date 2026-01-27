# Specification: Prevent Aggregate Self-Subscription Anti-Pattern

**Spec ID:** 0008  
**Status:** ✅ Implemented  
**Created:** 2026-01-26  
**Completed:** 2026-01-26  

## Problem Statement

Race condition occurred when aggregates subscribed to event bus as projection handlers for their own events, causing duplicate writes with stale data overwrites.

### Observed Behavior (Real-World)

In production application (catacloud), after submitting job:
1. `JobMachineSet` event processed correctly by aggregate
2. Saga sets `machine_id` on job
3. Query shows `jobs.machine_id` returns NULL (stale data)

### The Anti-Pattern

```rust
let job_aggregate = JobAggregate::new(...);

// ANTI-PATTERN: Subscribe aggregate to its own events
event_bus.subscribe(ProjectionHandler::new(job_aggregate.clone())).await?;
```

Intent was to ensure projection state updated before sagas query it. **This is unnecessary and harmful.**

### The Dual Write Problem

When aggregate subscribed as projection, entity persisted **twice** per event:

1. **First write (correct):** In `Aggregate::handle()` after processing command
2. **Second write (potentially stale):** In `ProjectionHandler::on_event()` from bus

### The Race Condition

Events processed concurrently by handlers:

```
Time    Action                                  Database State
────────────────────────────────────────────────────────────────
t1      Event v3 (JobSubmitted) created             
t2      Aggregate persists v3                   v3, machine_id=NULL
t3      Event v4 (JobMachineSet) created            
t4      Aggregate persists v4                   v4, machine_id=xxx ✓
t5      ProjectionHandler processes v3 (slow)       
t6      ProjectionHandler persists v3           v3, machine_id=NULL ✗
```

At t6, projection handler's write for v3 **overwrites** correct v4 state.

## Solution Overview

Refactored trait hierarchy so `Aggregate` does NOT implement `Projection`, providing **compile-time prevention**.

### Before (Problem)
```
Projection<ED>
    ↑
    │ extends (IS-A)
    │
Aggregate<ED>

Problem: Aggregate IS-A Projection, can wrap in ProjectionHandler
```

### After (Solution)
```
EventApplicator<ED>  ← Base trait with shared behavior
    ↑           ↑
    │           │
Projection   Aggregate

Solution: Separate traits, ProjectionHandler only accepts Projection
```

## Key Design Decisions

### Why Split the Hierarchy?

Extract shared behavior (event application, re-hydration) into `EventApplicator` base trait:
- `Projection` extends `EventApplicator` + adds `apply_and_store()`
- `Aggregate` extends `EventApplicator` + adds command handling
- Neither extends the other

### Why This Pattern is Unnecessary

Epoch already guarantees state persisted before events published:

```rust
// In Aggregate::handle()
for event in events {
    self.event_store.store_event(event).await?;  // Publishes to bus
}
state_store.persist_state(state).await?;  // State already saved
```

When saga receives event, aggregate's state already persisted. Subscribing aggregate as projection is redundant.

### Type Safety

`ProjectionHandler::new()` requires `P: Projection`, and `Aggregate` doesn't implement `Projection`:

```rust
// Compile error:
event_bus.subscribe(ProjectionHandler::new(aggregate)).await?;
//                                           ^^^^^^^^^ doesn't implement Projection
```

## Trait Definitions

```rust
/// Base trait for applying events to state
trait EventApplicator<ED> {
    type State: EventApplicatorState;
    type StateStore: StateStoreBackend<Self::State>;
    type EventType: TryFrom<&ED>;
    type ApplyError;
    
    fn apply(&self, state: Option<Self::State>, event: &Event<Self::EventType>) 
        -> Result<Option<Self::State>, Self::ApplyError>;
    fn get_state_store(&self) -> Self::StateStore;
    fn re_hydrate(...) -> ...;         // Default impl
    fn re_hydrate_from_refs(...) -> ...;  // Default impl
    fn get_id_from_event(...) -> Uuid;     // Default impl
}

/// Projection for event bus subscription
trait Projection<ED>: EventApplicator<ED> + SubscriberId {
    async fn apply_and_store(&self, event: &Event<ED>) -> Result<...> {
        // Default impl using apply() + state_store
    }
}

/// Aggregate for command handling
trait Aggregate<ED>: EventApplicator<ED> {
    type CommandData;
    type Command: TryFrom<CommandData>;
    // ... command handling methods
}
```

## Breaking Changes

- `Aggregate` no longer implements `Projection`
- `ProjectionState` renamed to `EventApplicatorState`
- `Projection::ProjectionError` renamed to `EventApplicator::ApplyError`
- Aggregates implement `EventApplicator` instead of `Projection`
- Projections need explicit `impl Projection<ED> for T {}` (can be empty, uses defaults)

## Migration Example

**Aggregate Before:**
```rust
impl Projection<ApplicationEvent> for UserAggregate {
    type State = User;
    type ProjectionError = UserError;
    // ...
}
impl Aggregate<ApplicationEvent> for UserAggregate { }
```

**Aggregate After:**
```rust
impl EventApplicator<ApplicationEvent> for UserAggregate {
    type State = User;
    type ApplyError = UserError;
    // ...
}
impl Aggregate<ApplicationEvent> for UserAggregate { }
// No SubscriberId - aggregates aren't subscribers
```

**Projection Before:**
```rust
impl Projection<ApplicationEvent> for ProductProjection {
    type State = Product;
    type ProjectionError = ProductError;
    // ...
}
impl SubscriberId for ProductProjection {
    fn subscriber_id(&self) -> &str { "projection:product" }
}
```

**Projection After:**
```rust
impl EventApplicator<ApplicationEvent> for ProductProjection {
    type State = Product;
    type ApplyError = ProductError;
    // ...
}
impl Projection<ApplicationEvent> for ProductProjection {}  // Enable ProjectionHandler
impl SubscriberId for ProductProjection {
    fn subscriber_id(&self) -> &str { "projection:product" }
}
```

## Compile-Fail Test

Added `trybuild` test to verify anti-pattern prevented:

```rust
// tests/compile_fail/aggregate_as_projection.rs
fn main() {
    let aggregate = MyAggregate::new();
    let handler = ProjectionHandler::new(aggregate);  // Should fail
}
```

## Results

- **Compile-time safety:** Impossible to subscribe aggregate to bus
- **Clear semantics:** Projection = subscribable, Aggregate = handles commands
- **Shared code:** `EventApplicator` contains common logic
- **No runtime overhead:** Pure type-system change
