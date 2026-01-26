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
    event::{Event, EventData},
    event_applicator::EventApplicator,
    event_store::EventObserver,
    state_store::StateStoreBackend,
};
use async_trait::async_trait;

/// `ApplyAndStoreError` enumerates the possible errors that can occur during the application
/// and storage of a projection's state.
///
/// Note: Conversion failures (when converting from a superset event to the projection's
/// subset event type) are intentionally ignored in `apply_and_store()`. This is expected
/// behavior since projections only handle events from their subset.
#[derive(Debug, thiserror::Error)]
pub enum ApplyAndStoreError<E, S> {
    /// Represents an error that occurred while applying an event to the projection.
    #[error("Event application error: {0}")]
    Event(E),
    /// Represents an error that occurred while persisting the projection's state to storage.
    #[error("Error persisting state: {0}")]
    State(S),
}

/// A read model that subscribes to the event bus to maintain derived state.
///
/// Unlike [`Aggregate`](crate::aggregate::Aggregate), a `Projection` is designed to be
/// subscribed to an event bus via [`ProjectionHandler`]. It receives events asynchronously
/// and updates its state.
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
/// // Implement Projection to enable subscription via ProjectionHandler
/// impl Projection<ApplicationEvent> for ProductProjection {}
/// ```
#[async_trait]
pub trait Projection<ED>: EventApplicator<ED>
where
    ED: EventData + Send + Sync + 'static,
{
    /// Applies an event received from the event bus and persists the resulting state.
    ///
    /// This method is called by [`ProjectionHandler`] when events are received.
    /// It retrieves the current state, applies the event, and persists the result.
    ///
    /// Takes a reference to the event to avoid unnecessary cloning.
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
        >,
    > {
        if let Ok(event) = event.to_subset_event_ref::<Self::EventType>() {
            let id = self.get_id_from_event(&event);
            let mut storage = self.get_state_store();
            let state = storage
                .get_state(id)
                .await
                .map_err(ApplyAndStoreError::State)?;
            if let Some(new_state) = self
                .apply(state, &event)
                .map_err(ApplyAndStoreError::Event)?
            {
                log::debug!("Persisting state for projection: {:?}", id);
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
{
    async fn on_event(
        &self,
        event: Arc<Event<ED>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.0.apply_and_store(&event).await?;
        Ok(())
    }
}
