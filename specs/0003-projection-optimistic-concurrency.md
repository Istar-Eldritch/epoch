# Specification: Projection Optimistic Concurrency Control

**Status: Implemented**

## Problem Statement

The `Projection::apply_and_store` method has a potential race condition when multiple events for the same entity are processed concurrently. The current implementation follows a non-atomic sequence:

1. `get_state(id)` - Read current state
2. `apply(state, event)` - Apply event to state
3. `persist_state(id, new_state)` - Write new state

If two events for the same entity arrive concurrently, both may read the same initial state, apply their respective events, and then persist—resulting in the second write overwriting the first event's changes. This leads to **lost updates**.

### Example Scenario

```
Time    Thread A (Event v2)           Thread B (Event v3)
────────────────────────────────────────────────────────────
T1      get_state() → v1              
T2                                    get_state() → v1
T3      apply(v1, event_v2) → v2
T4                                    apply(v1, event_v3) → v3'
T5      persist_state(v2)
T6                                    persist_state(v3')  ← WRONG! Should be v3 based on v2
```

The result is that Event v2's changes are lost because Thread B applied Event v3 to state v1 instead of v2.

## Solution: Re-hydration from Event Store

The `Aggregate::handle` method already solves this problem elegantly by re-hydrating from the event store before processing. We should apply the same pattern to projections.

The key insight is that the event store is the source of truth. By re-hydrating from the event store after fetching state from the state store, we ensure we have the latest state before applying the new event.

### How Aggregate Does It

```rust
// From Aggregate::handle
let state = state_store.get_state(state_id).await?;
let state_version = state.as_ref().map(|s| s.get_version()).unwrap_or(0);

// Re-hydrate from event store to catch up with any events that occurred
// after the state was persisted.
let state = if let Some(state) = state {
    let stream = event_store
        .read_events_since(state_id, state_version + 1)
        .await?;
    self.re_hydrate(Some(state), stream).await?
} else {
    None
};
```

### Applying the Same Pattern to Projection

For projections, we need to:

1. Get state from state store (as a cache/snapshot)
2. Re-hydrate from event store to catch up with any events since the snapshot
3. Apply the new event
4. Persist the updated state

This ensures that even if the state store is stale, we always have the correct state before applying the event.

## Proposed Changes

### 1. Add `ProjectionState` Trait (epoch_core/src/projection.rs)

Similar to `AggregateState`, projections need version tracking to know where to start re-hydration:

```rust
/// State for projections with version tracking for re-hydration.
///
/// This trait extends `EventApplicatorState` with version information used
/// to determine where to start re-hydrating from the event store.
pub trait ProjectionState: EventApplicatorState {
    /// Returns the current version of the projection state.
    /// This corresponds to the `stream_version` of the last applied event.
    fn get_version(&self) -> u64;
    
    /// Sets the version of the projection state.
    fn set_version(&mut self, version: u64);
}
```

### 2. Add EventStore to Projection Trait

The projection needs access to an event store for re-hydration:

```rust
#[async_trait]
pub trait Projection<ED>: EventApplicator<ED>
where
    ED: EventData + Send + Sync + 'static,
    <Self as EventApplicator<ED>>::State: ProjectionState,
{
    /// The event store backend for re-hydrating state.
    type EventStore: EventStoreBackend<EventType = ED> + Send + Sync + 'static;
    
    /// Returns an instance of the event store.
    fn get_event_store(&self) -> Self::EventStore;
    
    // ... rest of trait
}
```

### 3. Updated `apply_and_store` Implementation

```rust
async fn apply_and_store(
    &self,
    event: &Event<ED>,
) -> Result<
    (),
    ApplyAndStoreError<
        Self::ApplyError,
        <Self::StateStore as StateStoreBackend<Self::State>>::Error,
        <Self::EventStore as EventStoreBackend>::Error,
    >,
> {
    if let Ok(subset_event) = event.to_subset_event_ref::<Self::EventType>() {
        let id = self.get_id_from_event(&subset_event);
        let mut storage = self.get_state_store();
        
        // Step 1: Get state from state store (may be stale)
        let state = storage
            .get_state(id)
            .await
            .map_err(ApplyAndStoreError::State)?;
        
        let state_version = state.as_ref().map(|s| s.get_version()).unwrap_or(0);
        
        // Step 2: Re-hydrate from event store to catch up with any events
        // that occurred after the state was persisted
        let state = if let Some(state) = state {
            let event_store = self.get_event_store();
            let stream = event_store
                .read_events_since(id, state_version + 1)
                .await
                .map_err(ApplyAndStoreError::EventStore)?;
            self.re_hydrate(Some(state), stream)
                .await
                .map_err(ApplyAndStoreError::Hydration)?
        } else {
            // No state exists - re-hydrate from the beginning
            let event_store = self.get_event_store();
            let stream = event_store
                .read_events(id)
                .await
                .map_err(ApplyAndStoreError::EventStore)?;
            self.re_hydrate(None, stream)
                .await
                .map_err(ApplyAndStoreError::Hydration)?
        };
        
        // Step 3: Apply the new event
        if let Some(mut new_state) = self
            .apply(state, &subset_event)
            .map_err(ApplyAndStoreError::Event)?
        {
            // Update version to match the event's stream_version
            new_state.set_version(event.stream_version);
            
            log::debug!("Persisting state for projection: {:?} (version {})", id, event.stream_version);
            storage
                .persist_state(id, new_state)
                .await
                .map_err(ApplyAndStoreError::State)?;
        } else {
            log::debug!("Deleting state for projection: {:?}", id);
            storage
                .delete_state(id)
                .await
                .map_err(ApplyAndStoreError::State)?;
        }
    }
    Ok(())
}
```

### 4. Updated Error Type

```rust
#[derive(Debug, thiserror::Error)]
pub enum ApplyAndStoreError<E, S, ES> {
    /// Represents an error that occurred while applying an event to the projection.
    #[error("Event application error: {0}")]
    Event(E),
    /// Represents an error that occurred while persisting the projection's state to storage.
    #[error("Error persisting state: {0}")]
    State(S),
    /// Represents an error reading from the event store.
    #[error("Error reading from event store: {0}")]
    EventStore(ES),
    /// Represents an error during re-hydration.
    #[error("Error during re-hydration: {0}")]
    Hydration(String),
}
```

## Files to Modify

| File | Changes |
|------|---------|
| `epoch_core/src/projection.rs` | Add `ProjectionState` trait, add `EventStore` associated type, update `apply_and_store`, update error type |

## Backwards Compatibility

This is a **breaking change** for existing projections:

1. Projection state must now implement `ProjectionState` (adding `get_version`/`set_version`)
2. Projection must provide an `EventStore` type and `get_event_store()` method
3. The `ApplyAndStoreError` type gains additional generic parameters

## Benefits

1. **Consistent with Aggregate pattern** - Same re-hydration approach used throughout the framework
2. **Simple implementation** - No retry logic or version conflict handling needed
3. **Correct by construction** - Always starts from the correct state
4. **No state store changes** - The `StateStoreBackend` trait doesn't need modification

## Trade-offs

1. **Additional I/O** - Each `apply_and_store` now reads from the event store
2. **Requires event store access** - Projections must have access to the event store, not just the event bus

## Testing Strategy

1. **Unit test** for re-hydration in `apply_and_store`
2. **Integration test** simulating concurrent event processing to verify no lost updates
3. **Test** that version is correctly updated after applying events
