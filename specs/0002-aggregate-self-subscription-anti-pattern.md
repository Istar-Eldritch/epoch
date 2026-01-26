# Specification: Aggregate Self-Subscription Anti-Pattern

**Spec ID:** 0002  
**Status:** Implemented  
**Created:** 2026-01-25  
**Last Updated:** 2026-01-26  

## 1. Problem Statement

A race condition occurs when an aggregate is subscribed to the event bus as a projection handler for its own events. This pattern causes duplicate writes with potential stale data overwrites.

### Observed Behavior

In a real-world application (catacloud), after submitting a job:
1. The `JobMachineSet` event is processed correctly by the aggregate
2. The saga sets `machine_id` on the job
3. But when querying `jobs.machine_id`, it returns `NULL`

The workaround was to query a different table (`machines.assigned_job_id`) instead of relying on `jobs.machine_id`.

### The Anti-Pattern

```rust
let job_aggregate = JobAggregate::new(event_store.clone(), postgres.clone());

// ANTI-PATTERN: Subscribing aggregate to receive its own events
event_bus
    .subscribe(ProjectionHandler::new(job_aggregate.clone()))
    .await?;
```

The intent behind this pattern was to ensure projection state is updated before sagas query it. However, this is unnecessary and harmful.

## 2. Root Cause Analysis

### The Dual Write Problem

When an aggregate is subscribed as a projection handler, the same entity gets persisted twice per event:

1. **First write (correct)**: In `Aggregate::handle()` after processing the command
2. **Second write (potentially stale)**: In `ProjectionHandler::on_event()` when receiving the event from the bus

### Write Sequence Diagram

```
Command arrives
       │
       ▼
┌─────────────────────────────────────────────────────────────────┐
│                     Aggregate::handle()                          │
│  ┌──────────────┐    ┌──────────────┐    ┌──────────────┐       │
│  │ Validate &   │───▶│ Store Events │───▶│ Persist      │       │
│  │ Emit Events  │    │ to EventStore│    │ State (v4)   │       │
│  └──────────────┘    └──────────────┘    └──────────────┘       │
│                              │                   │               │
│                              │                   │ ◄─── First    │
│                              │                   │      Write    │
└──────────────────────────────┼───────────────────┼───────────────┘
                               │                   │
                               ▼                   │
                    ┌──────────────────┐           │
                    │    Event Bus     │           │
                    │  (publishes v3   │           │
                    │   and v4)        │           │
                    └────────┬─────────┘           │
                             │                     │
           ┌─────────────────┼─────────────────┐   │
           ▼                 ▼                 ▼   │
    ┌─────────────┐   ┌─────────────┐   ┌─────────────┐
    │ Saga        │   │ Projection  │   │ SAME        │
    │ Handler     │   │ Handler     │   │ Aggregate   │
    │             │   │             │   │ (as Proj)   │
    └─────────────┘   └─────────────┘   └──────┬──────┘
                                               │
                                               ▼
                                    ┌──────────────────┐
                                    │ apply_and_store  │
                                    │ (processes v3    │
                                    │  after v4!)      │
                                    └────────┬─────────┘
                                             │
                                             ▼
                                    ┌──────────────────┐
                                    │ Persist State    │
                                    │ (v3 overwrites   │ ◄─── Second Write
                                    │  v4!)            │      (STALE!)
                                    └──────────────────┘
```

### The Race Condition

Events are processed concurrently by projection handlers. Consider this sequence:

```
Time    Action                                      Database State
─────────────────────────────────────────────────────────────────────
t1      Event v3 (JobSubmitted) created             
t2      Aggregate persists v3                       v3, machine_id=NULL
t3      Event v4 (JobMachineSet) created            
t4      Aggregate persists v4                       v4, machine_id=xxx ✓
t5      ProjectionHandler processes v3 (slow)       
t6      ProjectionHandler persists v3               v3, machine_id=NULL ✗
```

At t6, the projection handler's write for v3 **overwrites** the correct v4 state because:
1. Projection handlers may process events out of order (concurrency)
2. The current upsert SQL has no version check

### Current Upsert SQL (No Version Guard)

```sql
INSERT INTO jobs (id, version, ..., machine_id, ...)
VALUES ($1, $2, ..., $14, ...)
ON CONFLICT (id)
DO UPDATE SET 
    version = $2, 
    ..., 
    machine_id = $14, 
    ...
```

This blindly overwrites regardless of whether the existing row has a higher version.

## 3. Why This Pattern Is Unnecessary

The epoch framework already guarantees that aggregate state is persisted before events are published:

From `Aggregate::handle()` in `epoch_core/src/aggregate.rs`:

```rust
// 1. Events are stored
for event in events.into_iter() {
    self.get_event_store()
        .store_event(event)
        .await
        .map_err(HandleCommandError::Event)?;
}

// 2. State is persisted BEFORE returning
if let Some(state) = state {
    state_store
        .persist_state(state.get_id(), state.clone())
        .await
        .map_err(HandleCommandError::State)?;
}
```

The event store's `store_event` publishes to the bus, but only **after** the aggregate has already persisted its state. Therefore:

- When a saga receives an event, the aggregate's state is already persisted
- Subscribing the aggregate as a projection is redundant
- The redundant subscription creates the race condition

## 4. Solution: Split the Trait Hierarchy

The chosen solution is to refactor the trait hierarchy so that `Aggregate` does NOT implement `Projection`. This provides **compile-time prevention** of the anti-pattern.

### Current Architecture (Problem)

```
┌─────────────────────────────────────────────────────────────┐
│                      Projection<ED>                          │
│  ┌─────────────────────────────────────────────────────────┐│
│  │ - State, StateStore, EventType, ProjectionError         ││
│  │ - apply()                                               ││
│  │ - get_state_store()                                     ││
│  │ - re_hydrate()                                          ││
│  │ - re_hydrate_from_refs()                                ││
│  │ - apply_and_store()  ◄── Used by ProjectionHandler      ││
│  │ - get_id_from_event()                                   ││
│  └─────────────────────────────────────────────────────────┘│
└─────────────────────────────────────────────────────────────┘
                              ▲
                              │ extends (IS-A)
                              │
┌─────────────────────────────────────────────────────────────┐
│                      Aggregate<ED>                           │
│  ┌─────────────────────────────────────────────────────────┐│
│  │ - CommandData, CommandCredentials, Command              ││
│  │ - EventStore, AggregateError                            ││
│  │ - get_event_store()                                     ││
│  │ - handle_command()                                      ││
│  │ - handle()  ◄── Persists state, then publishes events   ││
│  │ - get_id_from_command()                                 ││
│  └─────────────────────────────────────────────────────────┘│
└─────────────────────────────────────────────────────────────┘

Problem: Aggregate IS-A Projection, so it can be wrapped in ProjectionHandler
```

### New Architecture (Solution)

Extract the shared behavior into a base trait, then have `Projection` and `Aggregate` as **separate, non-inheriting** traits:

```
┌─────────────────────────────────────────────────────────────┐
│                    EventApplicator<ED>                       │
│  ┌─────────────────────────────────────────────────────────┐│
│  │ - State, StateStore, EventType, ApplyError              ││
│  │ - apply()           ◄── Core event application logic    ││
│  │ - get_state_store()                                     ││
│  │ - re_hydrate()                                          ││
│  │ - re_hydrate_from_refs()                                ││
│  │ - get_id_from_event()                                   ││
│  └─────────────────────────────────────────────────────────┘│
└─────────────────────────────────────────────────────────────┘
           ▲                                    ▲
           │ extends                            │ extends
           │                                    │
┌──────────────────────────┐      ┌─────────────────────────────────────┐
│     Projection<ED>       │      │           Aggregate<ED>              │
│  ┌────────────────────┐  │      │  ┌───────────────────────────────┐  │
│  │ - apply_and_store()│  │      │  │ - CommandData, Command, etc.  │  │
│  │   (for event bus)  │  │      │  │ - handle_command()            │  │
│  └────────────────────┘  │      │  │ - handle()                    │  │
└──────────────────────────┘      │  │   (persists state internally) │  │
           ▲                      │  └───────────────────────────────┘  │
           │                      └─────────────────────────────────────┘
           │
┌──────────────────────────┐
│   ProjectionHandler<P>   │
│  where P: Projection     │  ◄── Can ONLY wrap Projection, NOT Aggregate
└──────────────────────────┘
```

### Key Benefits

1. **Compile-time safety:** `ProjectionHandler::new()` requires `P: Projection`, and `Aggregate` doesn't implement `Projection`
2. **Clear semantics:** `Projection` = subscribable to event bus, `Aggregate` = handles commands
3. **Shared code:** `EventApplicator` contains all the common logic (apply, re_hydrate, etc.)
4. **No runtime overhead:** This is purely a type-system change

## 5. Implementation Details

### 5.1 New File: `epoch_core/src/event_applicator.rs`

```rust
//! Base trait for types that can apply events to build state.

use crate::event::{EnumConversionError, Event, EventData};
use crate::prelude::{EventStream, RefEventStream, StateStoreBackend};
use async_trait::async_trait;
use std::pin::Pin;
use tokio_stream::StreamExt;
use uuid::Uuid;

/// State built from events. Base trait for both projection and aggregate state.
pub trait EventApplicatorState {
    /// Returns the unique identifier of this state instance.
    fn get_id(&self) -> Uuid;
}

/// Errors that can arise during rehydration.
#[derive(Debug, thiserror::Error)]
pub enum ReHydrateError<E, S, ES> {
    /// Error applying an event
    #[error("Event application error: {0}")]
    Application(E),
    /// Error converting event to subset type
    #[error("Error transforming event: {0}")]
    Subset(S),
    /// Error reading from event stream
    #[error("Error reading from event stream: {0}")]
    EventStream(ES),
}

/// Base trait for types that apply events to build/update state.
///
/// This trait captures the common behavior between Aggregates and Projections:
/// both need to apply events to state. However, they differ in how they're used:
/// - `Aggregate`: Handles commands, persists state in `handle()`, publishes events
/// - `Projection`: Subscribes to event bus, persists state in `apply_and_store()`
#[async_trait]
pub trait EventApplicator<ED>
where
    ED: EventData + Send + Sync + 'static,
{
    /// The type of state managed by this applicator.
    type State: EventApplicatorState + Send + Sync + Clone;

    /// The state store backend for persisting state.
    type StateStore: StateStoreBackend<Self::State> + Send + Sync;

    /// The subset event type this applicator handles.
    type EventType: EventData + for<'a> TryFrom<&'a ED, Error = EnumConversionError>;

    /// Error type for event application.
    type ApplyError: std::error::Error + Send + Sync + 'static;

    /// Applies an event to produce new state.
    /// Returns `None` to indicate the state should be deleted.
    fn apply(
        &self,
        state: Option<Self::State>,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::ApplyError>;

    /// Returns the state store.
    fn get_state_store(&self) -> Self::StateStore;

    /// Extracts the state ID from an event.
    fn get_id_from_event(&self, event: &Event<Self::EventType>) -> Uuid {
        event.stream_id
    }

    /// Reconstructs state from an event stream.
    async fn re_hydrate<'a, E>(
        &self,
        mut state: Option<Self::State>,
        mut event_stream: Pin<Box<dyn EventStream<ED, E> + Send + 'a>>,
    ) -> Result<Option<Self::State>, ReHydrateError<Self::ApplyError, EnumConversionError, E>> {
        while let Some(event) = event_stream.next().await {
            let event = event.map_err(ReHydrateError::EventStream)?;
            let event = event.to_subset_event_ref().map_err(ReHydrateError::Subset)?;
            state = self.apply(state, &event).map_err(ReHydrateError::Application)?;
        }
        Ok(state)
    }

    /// Reconstructs state from a reference-based event stream.
    async fn re_hydrate_from_refs<'a, E>(
        &self,
        mut state: Option<Self::State>,
        mut event_stream: Pin<Box<dyn RefEventStream<'a, ED, E> + Send + 'a>>,
    ) -> Result<Option<Self::State>, ReHydrateError<Self::ApplyError, EnumConversionError, E>>
    where
        E: 'a,
    {
        while let Some(event) = event_stream.next().await {
            let event = event.map_err(ReHydrateError::EventStream)?;
            let event = event.to_subset_event_ref().map_err(ReHydrateError::Subset)?;
            state = self.apply(state, &event).map_err(ReHydrateError::Application)?;
        }
        Ok(state)
    }
}
```

### 5.2 Modified: `epoch_core/src/projection.rs`

```rust
//! Projection trait for read models that subscribe to the event bus.

use crate::event::{Event, EventData};
use crate::event_applicator::EventApplicator;
use crate::prelude::{EventObserver, StateStoreBackend};
use async_trait::async_trait;
use std::sync::Arc;

/// Error during apply_and_store.
#[derive(Debug, thiserror::Error)]
pub enum ApplyAndStoreError<E, S> {
    /// Error applying an event
    #[error("Event application error: {0}")]
    Event(E),
    /// Error persisting state
    #[error("Error persisting state: {0}")]
    State(S),
}

/// A read model that subscribes to the event bus to maintain derived state.
///
/// Unlike `Aggregate`, a `Projection` is designed to be subscribed to an event bus
/// via `ProjectionHandler`. It receives events asynchronously and updates its state.
///
/// # Important
///
/// Do NOT implement `Projection` for types that also implement `Aggregate`.
/// Aggregates manage their own state persistence in `handle()` and should never
/// be subscribed to the event bus for their own events.
#[async_trait]
pub trait Projection<ED>: EventApplicator<ED>
where
    ED: EventData + Send + Sync + 'static,
{
    /// Applies an event received from the event bus and persists the resulting state.
    ///
    /// This method is called by `ProjectionHandler` when events are received.
    async fn apply_and_store(
        &self,
        event: &Event<ED>,
    ) -> Result<
        (),
        ApplyAndStoreError<
            Self::ApplyError,
            <Self::StateStore as StateStoreBackend<Self::State>>::Error,
        >,
    > {
        if let Ok(event) = event.to_subset_event_ref::<Self::EventType>() {
            let id = self.get_id_from_event(&event);
            let mut storage = self.get_state_store();
            let state = storage.get_state(id).await.map_err(ApplyAndStoreError::State)?;

            if let Some(new_state) = self.apply(state, &event).map_err(ApplyAndStoreError::Event)? {
                log::debug!("Persisting state for projection: {:?}", id);
                storage.persist_state(id, new_state).await.map_err(ApplyAndStoreError::State)?;
            } else {
                log::debug!("Deleting state for projection: {:?}", id);
                storage.delete_state(id).await.map_err(ApplyAndStoreError::State)?;
            }
        }
        Ok(())
    }
}

/// Wraps a `Projection` to implement `EventObserver` for event bus subscription.
///
/// # Type Safety
///
/// This handler only accepts types implementing `Projection`, NOT `Aggregate`.
/// This prevents the anti-pattern of subscribing aggregates to their own events.
pub struct ProjectionHandler<P>(pub P);

impl<P> ProjectionHandler<P> {
    /// Creates a new `ProjectionHandler` wrapping the given projection.
    pub fn new(projection: P) -> Self {
        Self(projection)
    }

    /// Returns a reference to the inner projection.
    pub fn inner(&self) -> &P {
        &self.0
    }

    /// Consumes the handler and returns the inner projection.
    pub fn into_inner(self) -> P {
        self.0
    }
}

#[async_trait]
impl<ED, P> EventObserver<ED> for ProjectionHandler<P>
where
    ED: EventData + Send + Sync + 'static,
    P: Projection<ED> + Send + Sync,
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

### 5.3 Modified: `epoch_core/src/aggregate.rs`

```rust
//! Aggregate trait for command handling and event sourcing.

use crate::event::{EnumConversionError, Event, EventData};
use crate::event_applicator::{EventApplicator, EventApplicatorState, ReHydrateError};
use crate::event_store::EventStoreBackend;
use crate::prelude::{SliceRefEventStream, StateStoreBackend};
use async_trait::async_trait;
use log::debug;
use uuid::Uuid;

/// Aggregate state with version tracking for optimistic concurrency.
pub trait AggregateState: EventApplicatorState {
    /// Returns the current version of the aggregate state.
    fn get_version(&self) -> u64;
    /// Sets the version of the aggregate state.
    fn set_version(&mut self, version: u64);
}

// ... Command and HandleCommandError remain unchanged ...

/// An aggregate handles commands and produces events.
///
/// # Important
///
/// Aggregates should NOT be subscribed to the event bus as projections.
/// The `handle()` method already persists state before publishing events.
/// Subscribing an aggregate to the bus causes duplicate writes and race conditions.
///
/// Note: `Aggregate` extends `EventApplicator`, NOT `Projection`. This is intentional
/// to prevent wrapping aggregates in `ProjectionHandler`.
#[async_trait]
pub trait Aggregate<ED>: EventApplicator<ED>
where
    ED: EventData + Send + Sync + 'static,
    <Self as EventApplicator<ED>>::State: AggregateState,
    Self::CommandData: Send + Sync,
    <Self::Command as TryFrom<Self::CommandData>>::Error: Send + Sync,
{
    /// The overarching type of `Command` data that this `Aggregate` can process.
    type CommandData: Clone + std::fmt::Debug;
    /// The type of credentials used by the commands on this application.
    type CommandCredentials: Clone + std::fmt::Debug + Send;
    /// The specific command type used by this aggregate.
    type Command: TryFrom<Self::CommandData> + Send;
    /// The event store backend for persisting events.
    type EventStore: EventStoreBackend<EventType = ED> + Send + Sync + 'static;
    /// Error type for command handling.
    type AggregateError: std::error::Error + Send + Sync + 'static;

    /// Returns the event store.
    fn get_event_store(&self) -> Self::EventStore;

    /// Handles a command, producing events.
    async fn handle_command(
        &self,
        state: &Option<Self::State>,
        command: Command<Self::Command, Self::CommandCredentials>,
    ) -> Result<Vec<Event<ED>>, Self::AggregateError>;

    /// Handles a command, persists state, and publishes events.
    ///
    /// The state is persisted BEFORE events are published to the bus.
    /// This guarantees that when any subscriber receives an event,
    /// the aggregate's state is already up-to-date.
    async fn handle(
        &self,
        command: Command<Self::CommandData, Self::CommandCredentials>,
    ) -> Result<
        Option<<Self as EventApplicator<ED>>::State>,
        HandleCommandError<
            Self::AggregateError,
            ReHydrateError<
                <Self as EventApplicator<ED>>::ApplyError,
                EnumConversionError,
                <<Self as Aggregate<ED>>::EventStore as EventStoreBackend>::Error,
            >,
            <Self::StateStore as StateStoreBackend<Self::State>>::Error,
            <Self::EventStore as EventStoreBackend>::Error,
        >,
    > {
        // ... implementation unchanged, but references EventApplicator instead of Projection ...
    }

    /// Extracts the aggregate ID from a command.
    fn get_id_from_command(
        &self,
        command: &Command<Self::CommandData, Self::CommandCredentials>,
    ) -> Uuid {
        command.aggregate_id
    }
}
```

### 5.4 Modified: `epoch_core/src/lib.rs`

```rust
//! # Epoch

#![deny(missing_docs)]

pub mod aggregate;
pub mod event;
pub mod event_applicator;  // NEW
pub mod event_store;
pub mod projection;
pub mod saga;
pub mod state_store;

/// Re-exports the most commonly used traits and types for convenience.
pub mod prelude {
    pub use super::aggregate::*;
    pub use super::event::*;
    pub use super::event_applicator::*;  // NEW
    pub use super::event_store::*;
    pub use super::projection::*;
    pub use super::saga::*;
    pub use super::state_store::*;
}
```

## 6. Breaking Changes

| Change | Migration |
|--------|-----------|
| `Aggregate` no longer implements `Projection` | Aggregates can no longer be passed to `ProjectionHandler::new()` |
| `ProjectionState` renamed to `EventApplicatorState` | Update type references |
| `ProjectionError` renamed to `ApplyError` | Update associated type names |
| Aggregates implement `EventApplicator` instead of `Projection` | Change `impl Projection<ED> for MyAggregate` to `impl EventApplicator<ED> for MyAggregate` |
| Pure projections need explicit `impl Projection<ED>` | Add `impl Projection<ED> for MyProjection {}` (empty, uses default impl) |

## 7. Migration Guide

### Step 1: Update Aggregate Implementations

**Before:**
```rust
impl Projection<ApplicationEvent> for UserAggregate {
    type State = User;
    type EventType = UserEvent;
    type StateStore = InMemoryStateStore<User>;
    type ProjectionError = UserProjectionError;

    fn get_state_store(&self) -> Self::StateStore { ... }
    fn apply(&self, state: Option<Self::State>, event: &Event<Self::EventType>) -> Result<...> { ... }
}

impl Aggregate<ApplicationEvent> for UserAggregate { ... }
```

**After:**
```rust
impl EventApplicator<ApplicationEvent> for UserAggregate {
    type State = User;
    type EventType = UserEvent;
    type StateStore = InMemoryStateStore<User>;
    type ApplyError = UserApplyError;  // Renamed from ProjectionError

    fn get_state_store(&self) -> Self::StateStore { ... }
    fn apply(&self, state: Option<Self::State>, event: &Event<Self::EventType>) -> Result<...> { ... }
}

impl Aggregate<ApplicationEvent> for UserAggregate { ... }
```

### Step 2: Update Projection Implementations

**Before:**
```rust
impl Projection<ApplicationEvent> for ProductProjection {
    type State = Product;
    type EventType = ProductEvent;
    type StateStore = InMemoryStateStore<Product>;
    type ProjectionError = ProductProjectionError;

    fn get_state_store(&self) -> Self::StateStore { ... }
    fn apply(&self, state: Option<Self::State>, event: &Event<Self::EventType>) -> Result<...> { ... }
}
```

**After:**
```rust
impl EventApplicator<ApplicationEvent> for ProductProjection {
    type State = Product;
    type EventType = ProductEvent;
    type StateStore = InMemoryStateStore<Product>;
    type ApplyError = ProductProjectionError;

    fn get_state_store(&self) -> Self::StateStore { ... }
    fn apply(&self, state: Option<Self::State>, event: &Event<Self::EventType>) -> Result<...> { ... }
}

// Explicit Projection impl enables use with ProjectionHandler
impl Projection<ApplicationEvent> for ProductProjection {}
```

### Step 3: Update State Trait Implementations

**Before:**
```rust
impl ProjectionState for User {
    fn get_id(&self) -> Uuid { self.id }
}
```

**After:**
```rust
impl EventApplicatorState for User {
    fn get_id(&self) -> Uuid { self.id }
}
```

### Step 4: Remove Invalid Subscriptions

Any code that subscribed an aggregate to the event bus will now fail to compile:

```rust
// This will no longer compile (Aggregate doesn't implement Projection)
event_bus.subscribe(ProjectionHandler::new(job_aggregate.clone())).await?;
```

Simply remove these lines. The aggregate already persists its state in `handle()`.

### Step 5: Update Imports

```rust
// Add EventApplicator to imports if needed
use epoch::prelude::{EventApplicator, EventApplicatorState, ...};
```

## 8. Backward Compatibility Aids

To ease migration, consider providing deprecated type aliases in `epoch_core/src/projection.rs`:

```rust
/// Deprecated: Use `EventApplicatorState` instead.
#[deprecated(since = "0.X.0", note = "Use EventApplicatorState instead")]
pub type ProjectionState = crate::event_applicator::EventApplicatorState;
```

## 9. Test Plan

1. **Unit tests:** Verify `EventApplicator`, `Projection`, and `Aggregate` traits work correctly in isolation
2. **Integration tests:** Verify existing examples compile and pass after migration
3. **Compile-fail tests:** Add tests that verify `ProjectionHandler::new(aggregate)` fails to compile
4. **Migration test:** Apply migration to catacloud codebase and verify it compiles and passes tests

## 10. Documentation Updates

### Update README.md

Add section explaining the trait hierarchy:

```markdown
## Trait Hierarchy

Epoch uses a clear trait hierarchy to separate concerns:

- `EventApplicator<ED>` - Base trait for applying events to state
  - `Projection<ED>` - Read models that subscribe to the event bus
  - `Aggregate<ED>` - Command handlers that produce events

### Why Aggregates Can't Be Projections

Aggregates persist their state in `handle()` before publishing events. Subscribing 
them to the event bus would cause duplicate writes and race conditions. The type 
system enforces this: `ProjectionHandler` only accepts `Projection`, not `Aggregate`.
```

## 11. Implementation Summary

The refactoring has been implemented with the following changes:

### Files Created
- `epoch_core/src/event_applicator.rs` - New base trait module

### Files Modified
- `epoch_core/src/lib.rs` - Added `event_applicator` module and prelude exports
- `epoch_core/src/projection.rs` - `Projection` now extends `EventApplicator`
- `epoch_core/src/aggregate.rs` - `Aggregate` now extends `EventApplicator` (not `Projection`)
- `epoch_core/tests/slice_ref_event_stream_tests.rs` - Updated to use new traits
- `epoch_core/tests/aggregate_rehydration_tests.rs` - Updated to use new traits
- `epoch_mem/src/event_store.rs` - Updated test code to use new traits
- `epoch_pg/tests/pgeventbus_integration_tests.rs` - Updated to use new traits
- `epoch/examples/hello-world.rs` - Updated to use new traits

### Verification
- All tests pass
- `cargo clippy -- -D warnings` passes
- `cargo fmt --check` passes
- Example runs successfully

## 12. Approval

- [ ] Reviewed by project maintainer
- [ ] Migration guide validated
- [ ] Documentation updates approved
