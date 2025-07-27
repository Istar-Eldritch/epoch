//! This module defines traits for `Projection`
//! A `Projection` is a read-model built from a stream of events.

use crate::{
    event::{Event, EventData},
    prelude::EventObserver,
    state_store::StateStoreBackend,
};
use async_trait::async_trait;
use thiserror::Error;
use uuid::Uuid;

/// Errors that may happen when re_hydrating a projection
#[derive(Debug, Error)]
pub enum HydrationError {
    /// The state of the projection was not found
    #[error("State for ID {0} not found after rehydration")]
    StateNotFound(Uuid),
    /// Other errors
    #[error(transparent)]
    Other(#[from] Box<dyn std::error::Error + Send + Sync>),
}

/// `Projection` is a trait that defines the interface for a read-model that can be built
/// from a stream of events.
#[async_trait]
pub trait Projection<ED>
where
    ED: EventData + Send + Sync + 'static,
    <Self::CreateEvent as TryFrom<ED>>::Error: Send + Sync,
    <Self::UpdateEvent as TryFrom<ED>>::Error: Send + Sync,
    <Self::DeleteEvent as TryFrom<ED>>::Error: Send + Sync,
{
    /// The type of the state that this `Projection` manages.
    type State: Send + Sync + Clone;
    /// The type of `StateStorage` used by this `Projection`.
    type StateStore: StateStoreBackend<Self::State> + Send + Sync;
    /// The type of event that triggers a create operation on the state.
    type CreateEvent: EventData + TryFrom<ED>;
    /// The type of event that triggers an update operation on the state.
    type UpdateEvent: EventData + TryFrom<ED>;
    /// The type of event that triggers a delete operation on the state.
    type DeleteEvent: EventData + TryFrom<ED>;
    /// Applies a create event to produce a new state.
    fn apply_create(
        &self,
        event: &Event<Self::CreateEvent>,
    ) -> Result<Self::State, Box<dyn std::error::Error + Send + Sync>>;
    /// Applies an update event to an existing state.
    fn apply_update(
        &self,
        state: Self::State,
        event: &Event<Self::UpdateEvent>,
    ) -> Result<Self::State, Box<dyn std::error::Error + Send + Sync>>;
    /// Applies a delete event to an existing state. By default, this method returns `None`,
    /// which will result in the state being deleted from storage. If `Some(state)` is returned,
    /// the state will be persisted instead of deleted.
    fn apply_delete(
        &self,
        _state: Self::State,
        _event: &Event<Self::DeleteEvent>,
    ) -> Result<Option<Self::State>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(None)
    }
    /// Returns the `StateStorage` implementation for this `Projection`.
    fn get_storage(&self) -> Self::StateStore;

    /// Reconstructs the projection's state from an event stream.
    async fn re_hydrate(
        &self,
        id: Uuid,
        event_stream: impl Iterator<Item = Event<ED>> + Send + Sync,
    ) -> Result<Self::State, HydrationError> {
        let state_storage = self.get_storage();
        let mut state = state_storage.get_state(id).await?;

        for event in event_stream {
            if let Ok(create_event) = event.to_subset_event::<Self::CreateEvent>() {
                state = Some(self.apply_create(&create_event)?);
            } else if let Some(ref current_state) = state {
                if let Ok(update_event) = event.to_subset_event::<Self::UpdateEvent>() {
                    state = Some(self.apply_update(current_state.clone(), &update_event)?);
                } else if let Ok(delete_event) = event.to_subset_event::<Self::DeleteEvent>() {
                    state = self.apply_delete(current_state.clone(), &delete_event)?;
                }
            }
        }

        state.ok_or_else(|| HydrationError::StateNotFound(id))
    }

    /// Applies an event to the projection. This method dispatches the event to the appropriate
    /// `apply_create`, `apply_update`, or `apply_delete` method based on the event type.
    async fn apply(
        &self,
        event: Event<ED>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let id = self.get_id_from_event(&event);
        if let Ok(create_event) = event.to_subset_event::<Self::CreateEvent>() {
            let state = self.apply_create(&create_event)?;
            let mut storage = self.get_storage();
            storage.persist_state(id, state).await?;
        } else if let Ok(update_event) = event.to_subset_event::<Self::UpdateEvent>() {
            let mut storage = self.get_storage();
            if let Some(state) = storage.get_state(id).await? {
                let updated = self.apply_update(state, &update_event)?;
                storage.persist_state(id, updated).await?;
            }
        } else if let Ok(delete_event) = event.to_subset_event::<Self::DeleteEvent>() {
            let mut storage = self.get_storage();
            if let Some(state) = storage.get_state(id).await? {
                if let Some(deleted) = self.apply_delete(state, &delete_event)? {
                    storage.persist_state(id, deleted).await?;
                } else {
                    storage.delete_state(id).await?;
                }
            }
        }
        Ok(())
    }
    /// Returns the ID of the stream from the given event.
    fn get_id_from_event(&self, event: &Event<ED>) -> Uuid {
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
        self.apply(event).await?;
        Ok(())
    }
}
