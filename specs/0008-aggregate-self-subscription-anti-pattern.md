# Specification: Prevent Aggregate Self-Subscription Anti-Pattern

**Spec ID:** 0008  
**Status:** ✅ Implemented  
**Created:** 2026-01-26  
**Last Updated:** 2026-01-26  
**Implemented:** 2026-01-26  
**Related:** Previously implemented as commit `b072127` but reverted during rebase

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

### Current Codebase Allows This

The current trait hierarchy has `Aggregate<ED>: Projection<ED>`, which means any aggregate can be wrapped in `ProjectionHandler` and subscribed to the event bus. This should be prevented at compile time.

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

## 3. Why This Pattern Is Unnecessary

The epoch framework already guarantees that aggregate state is persisted before events are published:

From `Aggregate::handle()` in `epoch_core/src/aggregate.rs`:

```rust
// Events are stored (which publishes to bus)
for event in events.into_iter() {
    self.get_event_store()
        .store_event(event)
        .await
        .map_err(HandleCommandError::Event)?;
}

// State is persisted
if let Some(state) = state {
    state_store
        .persist_state(*state.get_id(), state.clone())
        .await
        .map_err(HandleCommandError::State)?;
}
```

Therefore:

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
    fn get_id(&self) -> &Uuid;
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

The `Projection` trait now extends `EventApplicator` instead of defining its own state/apply methods:

```rust
//! Projection trait for read models that subscribe to the event bus.

use crate::event::{Event, EventData};
use crate::event_applicator::EventApplicator;
use crate::prelude::{EventObserver, StateStoreBackend};
use crate::SubscriberId;
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
///
/// # Subscriber ID
///
/// Implementors must also implement [`SubscriberId`] to provide a unique identifier
/// for checkpoint tracking, multi-instance coordination, and dead letter queue
/// association. Use the `#[derive(SubscriberId)]` macro from `epoch_derive`.
#[async_trait]
pub trait Projection<ED>: EventApplicator<ED> + SubscriberId
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

impl<P> SubscriberId for ProjectionHandler<P>
where
    P: SubscriberId,
{
    fn subscriber_id(&self) -> &str {
        self.0.subscriber_id()
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

The `Aggregate` trait now extends `EventApplicator` (NOT `Projection`):

```rust
//! Aggregate trait for command handling and event sourcing.
//!
//! An [`Aggregate`] is a cluster of domain objects that can be treated as a single unit
//! for data changes. It is the consistency boundary for commands and events in an
//! event-sourced system.
//!
//! # Important
//!
//! Aggregates should NOT be subscribed to the event bus as projections.
//! The `handle()` method already persists state before publishing events.
//! Subscribing an aggregate to the bus causes duplicate writes and race conditions.
//!
//! Note: `Aggregate` extends [`EventApplicator`], NOT [`Projection`](crate::projection::Projection).
//! This is intentional to prevent wrapping aggregates in [`ProjectionHandler`](crate::projection::ProjectionHandler).

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
    // ... associated types and methods unchanged ...
    // References change from Projection<ED> to EventApplicator<ED>
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
pub mod subscriber_id;

pub use subscriber_id::SubscriberId;

/// Re-exports the most commonly used traits and types for convenience.
pub mod prelude {
    pub use super::aggregate::*;
    pub use super::event::*;
    pub use super::event_applicator::*;  // NEW
    pub use super::event_store::*;
    pub use super::projection::*;
    pub use super::saga::*;
    pub use super::state_store::*;
    pub use super::subscriber_id::*;
}
```

## 6. Breaking Changes

| Change | Migration |
|--------|-----------|
| `Aggregate` no longer implements `Projection` | Aggregates can no longer be passed to `ProjectionHandler::new()` |
| `ProjectionState` renamed to `EventApplicatorState` | Update type references |
| `Projection::ProjectionError` renamed to `EventApplicator::ApplyError` | Update associated type names |
| Aggregates implement `EventApplicator` instead of `Projection` | Change `impl Projection<ED> for MyAggregate` to `impl EventApplicator<ED> for MyAggregate` |
| Pure projections need explicit `impl Projection<ED>` | Add `impl Projection<ED> for MyProjection {}` (empty, uses default impl) |

## 7. Migration Guide

### Step 1: Update Aggregate Implementations

**Before:**
```rust
impl ProjectionState for User {
    fn get_id(&self) -> &Uuid { &self.id }
}

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
impl EventApplicatorState for User {
    fn get_id(&self) -> &Uuid { &self.id }
}

impl EventApplicator<ApplicationEvent> for UserAggregate {
    type State = User;
    type EventType = UserEvent;
    type StateStore = InMemoryStateStore<User>;
    type ApplyError = UserApplyError;  // Renamed from ProjectionError

    fn get_state_store(&self) -> Self::StateStore { ... }
    fn apply(&self, state: Option<Self::State>, event: &Event<Self::EventType>) -> Result<...> { ... }
}

impl Aggregate<ApplicationEvent> for UserAggregate { ... }
// Note: No SubscriberId needed for aggregates (they're not subscribers)
```

### Step 2: Update Projection Implementations

**Before:**
```rust
impl ProjectionState for Product {
    fn get_id(&self) -> &Uuid { &self.id }
}

impl epoch_core::SubscriberId for ProductProjection {
    fn subscriber_id(&self) -> &str { "projection:product" }
}

impl Projection<ApplicationEvent> for ProductProjection {
    type State = Product;
    type StateStore = InMemoryStateStore<Self::State>;
    type EventType = ProductEvent;
    type ProjectionError = ProductProjectionError;

    fn get_state_store(&self) -> Self::StateStore { ... }
    fn apply(&self, state: Option<Self::State>, event: &Event<Self::EventType>) -> Result<...> { ... }
}
```

**After:**
```rust
impl EventApplicatorState for Product {
    fn get_id(&self) -> &Uuid { &self.id }
}

impl epoch_core::SubscriberId for ProductProjection {
    fn subscriber_id(&self) -> &str { "projection:product" }
}

impl EventApplicator<ApplicationEvent> for ProductProjection {
    type State = Product;
    type StateStore = InMemoryStateStore<Self::State>;
    type EventType = ProductEvent;
    type ApplyError = ProductProjectionError;

    fn get_state_store(&self) -> Self::StateStore { ... }
    fn apply(&self, state: Option<Self::State>, event: &Event<Self::EventType>) -> Result<...> { ... }
}

// Explicit Projection impl enables use with ProjectionHandler
impl Projection<ApplicationEvent> for ProductProjection {}
```

### Step 3: Remove Invalid Subscriptions

Any code that subscribed an aggregate to the event bus will now fail to compile:

```rust
// This will no longer compile (Aggregate doesn't implement Projection)
event_bus.subscribe(ProjectionHandler::new(job_aggregate.clone())).await?;
```

Simply remove these lines. The aggregate already persists its state in `handle()`.

### Step 4: Update Imports

```rust
// Add EventApplicator to imports if needed
use epoch::prelude::{EventApplicator, EventApplicatorState, ...};
```

## 8. Backward Compatibility Aids

To ease migration, provide deprecated re-exports in `epoch_core/src/projection.rs`.

**Note:** Rust does not support trait aliases on stable. Instead, we use documentation and a wrapper approach:

```rust
// In epoch_core/src/projection.rs

/// Deprecated: Use `EventApplicatorState` from `epoch_core::event_applicator` instead.
///
/// This re-export exists for backward compatibility and will be removed in a future version.
#[deprecated(
    since = "0.X.0",
    note = "Use epoch_core::event_applicator::EventApplicatorState instead"
)]
pub use crate::event_applicator::EventApplicatorState as ProjectionState;

/// Deprecated: Use `ReHydrateError` from `epoch_core::event_applicator` instead.
#[deprecated(
    since = "0.X.0",
    note = "Use epoch_core::event_applicator::ReHydrateError instead"
)]
pub use crate::event_applicator::ReHydrateError;
```

For the `Projection` trait itself, since `Aggregate` previously implemented `Projection`, code that relied on this will fail to compile. This is intentional—compile errors guide users to remove the anti-pattern. The migration guide (Section 7) provides clear steps.

## 9. Test Plan

### Unit Tests
1. Verify `EventApplicator`, `Projection`, and `Aggregate` traits work correctly in isolation
2. Verify `ProjectionHandler` only accepts `Projection` types

### Integration Tests
1. Verify existing examples compile and pass after migration
2. Verify `epoch_core/tests/aggregate_rehydration_tests.rs` passes with new traits
3. Verify `epoch_core/tests/slice_ref_event_stream_tests.rs` passes with new traits
4. Verify `epoch_pg/tests/pgeventbus_integration_tests.rs` passes with new traits

### Compile-Fail Tests

Add tests using `trybuild` to verify `ProjectionHandler::new(aggregate)` fails to compile with a clear error message.

**New file:** `epoch_core/tests/compile_fail/aggregate_as_projection.rs`

```rust
//! This file should fail to compile, verifying the anti-pattern is prevented.

use epoch_core::prelude::*;

// ... minimal aggregate definition ...

fn main() {
    let aggregate = MyAggregate::new();
    // This should fail: Aggregate doesn't implement Projection
    let handler = ProjectionHandler::new(aggregate);
}
```

**New file:** `epoch_core/tests/compile_fail_tests.rs`

```rust
#[test]
fn compile_fail_tests() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/*.rs");
}
```

**Add to `epoch_core/Cargo.toml` dev-dependencies:**

```toml
[dev-dependencies]
trybuild = "1.0"
```

## 10. Files to Modify

| File | Change |
|------|--------|
| `epoch_core/src/event_applicator.rs` | **NEW** - Base trait module |
| `epoch_core/src/lib.rs` | Add `event_applicator` module, update prelude |
| `epoch_core/src/projection.rs` | Extend `EventApplicator`, move `ReHydrateError`, add deprecated re-exports |
| `epoch_core/src/aggregate.rs` | Extend `EventApplicator` (not `Projection`) |
| `epoch_core/Cargo.toml` | Add `trybuild` dev-dependency |
| `epoch_core/tests/aggregate_rehydration_tests.rs` | Update trait imports/impls |
| `epoch_core/tests/slice_ref_event_stream_tests.rs` | Update trait imports/impls |
| `epoch_core/tests/compile_fail_tests.rs` | **NEW** - Compile-fail test runner |
| `epoch_core/tests/compile_fail/aggregate_as_projection.rs` | **NEW** - Compile-fail test case |
| `epoch_mem/src/event_store.rs` | Update test code |
| `epoch_pg/tests/pgeventbus_integration_tests.rs` | Update trait imports/impls |
| `epoch/examples/hello-world.rs` | Update trait imports/impls |

## 11. Derive Macro Considerations

### Existing `SubscriberId` Macro

The existing `#[derive(SubscriberId)]` macro in `epoch_derive` generates implementations of `epoch_core::SubscriberId`. This macro remains unchanged and continues to work for projections.

**Important:** Aggregates do NOT need `SubscriberId` because they are not event bus subscribers. The `SubscriberId` trait is only required for types implementing `Projection` (which are wrapped in `ProjectionHandler` and subscribed to the event bus).

### No New Derive Macros Required

The `EventApplicator` trait requires implementing:
- `apply()` - Business logic, cannot be derived
- `get_state_store()` - Returns instance-specific store, cannot be derived
- Associated types - Must be specified by the user

Since all required methods contain domain-specific logic, a derive macro would not provide value. The trait's default implementations for `re_hydrate()`, `re_hydrate_from_refs()`, and `get_id_from_event()` already reduce boilerplate.

### Future Consideration: `#[derive(Projection)]` Attribute Macro

If boilerplate becomes a concern, a future enhancement could add an attribute macro that generates the empty `impl Projection<ED> for T {}` block:

```rust
// Potential future enhancement (NOT part of this spec)
#[derive(SubscriberId)]
#[projection(ApplicationEvent)]  // Generates: impl Projection<ApplicationEvent> for ProductProjection {}
struct ProductProjection { ... }
```

This is out of scope for this specification but could be added later if the pattern proves cumbersome.

## 12. Documentation Updates

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

## 13. Approval

- [x] Reviewed by project maintainer
- [x] Migration guide validated
- [x] Implementation plan approved
- [x] Documentation updates approved
- [x] Implementation complete
