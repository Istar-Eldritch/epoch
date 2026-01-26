//! Projection trait for read models that subscribe to the event bus.
//!
//! A [`Projection`] is a read-model built from a stream of events. Unlike
//! [`Aggregate`](crate::aggregate::Aggregate), a projection is designed to be
//! subscribed to an event bus via [`ProjectionHandler`] to receive events
//! asynchronously and update its state.
//!
//! # Important
//!
//! Do NOT implement `Projection` for types that also implement `Aggregate`.
//! Aggregates manage their own state persistence in `handle()` and should never
//! be subscribed to the event bus for their own events. The type system enforces
//! this separation: `ProjectionHandler` only accepts `Projection`, not `Aggregate`.

use std::sync::Arc;

use crate::{
    event::{EnumConversionError, Event, EventData},
    event_applicator::{EventApplicator, EventApplicatorState, ReHydrateError},
    event_store::{EventObserver, EventStoreBackend},
    state_store::StateStoreBackend,
};
use async_trait::async_trait;

/// State for projections with version tracking for re-hydration.
///
/// This trait extends [`EventApplicatorState`] with version information used
/// to determine where to start re-hydrating from the event store. This enables
/// optimistic concurrency control to prevent race conditions when multiple
/// events for the same entity are processed concurrently.
///
/// The version corresponds to the `stream_version` of the last applied event.
pub trait ProjectionState: EventApplicatorState {
    /// Returns the current version of the projection state.
    /// This corresponds to the `stream_version` of the last applied event.
    fn get_version(&self) -> u64;

    /// Sets the version of the projection state.
    fn set_version(&mut self, version: u64);
}

/// `ApplyAndStoreError` enumerates the possible errors that can occur during the application
/// and storage of a projection's state.
///
/// Note: Conversion failures (when converting from a superset event to the projection's
/// subset event type) are intentionally ignored in `apply_and_store()`. This is expected
/// behavior since projections only handle events from their subset.
#[derive(Debug, thiserror::Error)]
pub enum ApplyAndStoreError<E, S, ES> {
    /// Represents an error that occurred while applying an event to the projection.
    #[error("Event application error: {0}")]
    Event(E),
    /// Represents an error that occurred while persisting the projection's state to storage.
    #[error("Error persisting state: {0}")]
    State(S),
    /// Represents an error reading from the event store during re-hydration.
    #[error("Error reading from event store: {0}")]
    EventStore(ES),
    /// Represents an error during re-hydration.
    #[error("Error during re-hydration: {0}")]
    Hydration(ReHydrateError<E, EnumConversionError, ES>),
}

// TODO: Consider adding a compile-time negative trait bound check or marker trait
// to enforce that Projection cannot be implemented for Aggregate types. Currently the
// separation relies on documentation only. A future enhancement could use a sealed trait
// pattern or const assertion to provide compile-time errors.

/// A read model that subscribes to the event bus to maintain derived state.
///
/// Unlike [`Aggregate`](crate::aggregate::Aggregate), a `Projection` is designed to be
/// subscribed to an event bus via [`ProjectionHandler`]. It receives events asynchronously
/// and updates its state.
///
/// # Concurrency Safety
///
/// The `apply_and_store` method uses re-hydration from the event store to handle
/// race conditions when multiple events for the same entity arrive concurrently.
/// Before applying an event, it re-hydrates from the event store to ensure the
/// projection has the latest state, similar to how [`Aggregate::handle`](crate::aggregate::Aggregate::handle)
/// works.
///
/// # Important
///
/// Do NOT implement `Projection` for types that also implement `Aggregate`.
/// Aggregates manage their own state persistence in `handle()` and should never
/// be subscribed to the event bus for their own events.
///
/// # Example
///
/// ```ignore
/// use epoch_core::prelude::*;
///
/// struct ProductProjection { /* ... */ }
///
/// impl EventApplicator<ApplicationEvent> for ProductProjection {
///     type State = Product;
///     type EventType = ProductEvent;
///     type StateStore = InMemoryStateStore<Product>;
///     type ApplyError = ProductError;
///
///     fn get_state_store(&self) -> Self::StateStore { /* ... */ }
///     fn apply(&self, state: Option<Self::State>, event: &Event<Self::EventType>) -> Result<Option<Self::State>, Self::ApplyError> { /* ... */ }
/// }
///
/// impl Projection<ApplicationEvent> for ProductProjection {
///     type EventStore = InMemoryEventStore<InMemoryEventBus<ApplicationEvent>>;
///     
///     fn get_event_store(&self) -> Self::EventStore { /* ... */ }
/// }
/// ```
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

    /// Applies an event received from the event bus and persists the resulting state.
    ///
    /// This method is called by [`ProjectionHandler`] when events are received.
    /// It retrieves the current state, re-hydrates from the event store to catch up
    /// with any events that occurred after the state was persisted, applies the event,
    /// and persists the result.
    ///
    /// # Concurrency Safety
    ///
    /// This method handles race conditions by re-hydrating from the event store before
    /// applying events. This ensures the projection always starts from the latest state,
    /// preventing lost updates when multiple events for the same entity arrive concurrently.
    ///
    /// # Arguments
    ///
    /// * `event` - The event received from the event bus
    ///
    /// # Returns
    ///
    /// * `Ok(())` - The event was processed successfully (or was not applicable to this projection)
    /// * `Err(e)` - An error occurred while processing or persisting
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
            let event_store = self.get_event_store();

            // Step 1: Get state from state store (may be stale)
            let state = storage
                .get_state(id)
                .await
                .map_err(ApplyAndStoreError::State)?;

            let state_version = state.as_ref().map(|s| s.get_version()).unwrap_or(0);

            // Step 2: Re-hydrate from event store to catch up with any events
            // that occurred after the state was persisted, but before this event.
            // Only re-hydrate if there's a gap between state version and the incoming event.
            let state = if state_version + 1 < event.stream_version {
                let stream = event_store
                    .read_events_since(id, state_version + 1)
                    .await
                    .map_err(ApplyAndStoreError::EventStore)?;

                // Re-hydrate, stopping before we reach the current event's version
                let current_event_version = event.stream_version;
                self.re_hydrate_until::<<Self::EventStore as EventStoreBackend>::Error>(
                    state,
                    stream,
                    current_event_version,
                )
                .await
                .map_err(ApplyAndStoreError::Hydration)?
            } else {
                state
            };

            // Step 3: Apply the new event
            if let Some(mut new_state) = self
                .apply(state, &subset_event)
                .map_err(ApplyAndStoreError::Event)?
            {
                // Update version to match the event's stream_version
                new_state.set_version(event.stream_version);

                log::debug!(
                    "Persisting state for projection: {:?} (version {})",
                    id,
                    event.stream_version
                );
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
}

/// A wrapper type that provides an [`EventObserver`] implementation for [`Projection`] types.
///
/// This wrapper is used to subscribe projections to an event bus.
///
/// # Type Safety
///
/// This handler only accepts types implementing [`Projection`], NOT [`Aggregate`](crate::aggregate::Aggregate).
/// This prevents the anti-pattern of subscribing aggregates to their own events,
/// which causes race conditions and duplicate writes.
///
/// # Example
///
/// ```ignore
/// use epoch_core::projection::ProjectionHandler;
///
/// let projection = MyProjection::new();
/// let handler = ProjectionHandler::new(projection);
/// event_bus.subscribe(handler).await?;
/// ```
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
    <P as EventApplicator<ED>>::State: ProjectionState,
{
    async fn on_event(
        &self,
        event: Arc<Event<ED>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.0.apply_and_store(&event).await?;
        Ok(())
    }
}
