//! This module defines traits for `Projection`
//! A `Projection` is a read-model built from a stream of events.

use crate::{
    event::{Event, EventData},
    prelude::EventObserver,
    state_store::StateStoreBackend,
};
use async_trait::async_trait;
use uuid::Uuid;

/// `ProjectionState` represents the current state of a projection.
/// It encapsulates the current data derived from a sequence of events.
pub trait ProjectionState {
    /// Returns the unique identifier of the projection instance. This ID is used to retrieve and persist the projection's state and events.
    fn get_id(&self) -> Uuid;
}

/// `ApplyAndStoreError` enumerates the possible errors that can occur during the application
/// and storage of a projection's state.
#[derive(Debug, thiserror::Error)]
pub enum ApplyAndStoreError<E, S> {
    /// Represents an error that occurred while applying an event to the projection.
    #[error("Event application error: {0}")]
    Event(E),
    /// Represents an error that occurred while persisting the projection's state to storage.
    #[error("Error persisting state: {0}")]
    State(S),
}

/// `Projection` is a trait that defines the interface for a read-model that can be built
/// from a stream of events.
#[async_trait]
pub trait Projection<ED>
where
    ED: EventData + Send + Sync + 'static,
    <Self::EventType as TryFrom<ED>>::Error: Send + Sync,
{
    /// The type of the state that this `Projection` manages.
    type State: ProjectionState + Send + Sync + Clone;
    /// The type of `StateStorage` used by this `Projection`.
    type StateStore: StateStoreBackend<Self::State> + Send + Sync;
    /// The type of event used by this projection.
    type EventType: EventData + TryFrom<ED>;
    /// The type of errors that may occur whey applying events
    type ProjectionError: std::error::Error + Send + Sync + 'static;

    /// Applies an event to the aggregate.
    /// If None is returned it will result in the state being deleted from storage.
    /// If `Some(state)` is returned, the state will be persisted instead of deleted.
    fn apply(
        &self,
        _state: Option<Self::State>,
        _event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::ProjectionError>;

    /// Returns the `StateStorage` implementation for this `Projection`.
    fn get_state_store(&self) -> Self::StateStore;

    /// Reconstructs the projection's state from an event stream.
    fn re_hydrate<'a>(
        &self,
        mut state: Option<Self::State>,
        event_stream: impl Iterator<Item = &'a Event<ED>> + Send + Sync,
    ) -> Result<Option<Self::State>, Self::ProjectionError> {
        for event in event_stream {
            if let Ok(event) = event.to_subset_event::<Self::EventType>() {
                state = self.apply(state, &event)?;
            }
        }

        Ok(state)
    }

    /// Applies an event to the projection. This method dispatches the event to the appropriate
    /// `apply_create`, `apply_update`, or `apply_delete` method based on the event type.
    async fn apply_and_store(
        &self,
        event: Event<ED>,
    ) -> Result<
        (),
        ApplyAndStoreError<
            Self::ProjectionError,
            <Self::StateStore as StateStoreBackend<Self::State>>::Error,
        >,
    > {
        if let Ok(event) = event.to_subset_event::<Self::EventType>() {
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
                storage
                    .persist_state(id, new_state)
                    .await
                    .map_err(ApplyAndStoreError::State)?;
            } else {
                storage
                    .delete_state(id)
                    .await
                    .map_err(ApplyAndStoreError::State)?;
            }
        }
        Ok(())
    }
    /// Returns the ID of the stream from the given event.
    fn get_id_from_event(&self, event: &Event<Self::EventType>) -> Uuid {
        event.stream_id
    }
}

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
