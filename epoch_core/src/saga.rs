//! Saga definition

use crate::event::{Event, EventData};
use crate::prelude::{EventObserver, StateStoreBackend};
use async_trait::async_trait;
use uuid::Uuid;

/// Defines the errors that can happen when handling an event in a saga
#[derive(Debug, thiserror::Error)]
pub enum HandleEventError<S, E> {
    /// An error raised while handling the event
    #[error("Error while handling event: {0}")]
    Saga(S),
    /// An error raised by the state store
    #[error("Error while storing state: {0}")]
    State(E),
}

/// `Saga` is a trait that defines the interface for a saga in an event-sourced system.
/// A saga is a long-running business process that is coordinated by a series of local transactions.
#[async_trait]
pub trait Saga<ED>: EventObserver<ED>
where
    ED: EventData + Send + Sync + 'static,
    <Self::EventType as TryFrom<ED>>::Error: Send + Sync,
{
    /// The type of the state of the saga. This acts as a state machine. It should be defined as an
    /// enum
    type State: Into<u8> + From<u8> + Send + Sync + Default;
    /// The type of `StateStorage` used by this `Saga`.
    type StateStore: StateStoreBackend<u8> + Send + Sync;
    /// The type of errors that may occur when handling events in this `Saga`.
    type SagaError: std::error::Error + Send + Sync + 'static;
    /// The type of event used by this saga
    type EventType: EventData + TryFrom<ED>;

    /// Returns the `StateStorage` implementation for this `Saga`.
    fn get_state_store(&self) -> Self::StateStore;

    /// Handles an incoming event, potentially producing and dispatching commands
    async fn handle_event(
        &self,
        state: Self::State,
        event: Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::SagaError>;

    /// Returns the ID of the saga from the given event.
    fn get_id_from_event(&self, event: &Event<Self::EventType>) -> Uuid {
        event.stream_id
    }

    /// Processes an incoming event, applies it to the saga, and persists the resulting state.
    async fn process_event(
        &self,
        event: Event<ED>,
    ) -> Result<
        (),
        HandleEventError<Self::SagaError, <Self::StateStore as StateStoreBackend<u8>>::Error>,
    > {
        if let Ok(event) = event.to_subset_event::<Self::EventType>() {
            let id = self.get_id_from_event(&event);
            let mut storage = self.get_state_store();
            let state = storage
                .get_state(id)
                .await
                .map_err(HandleEventError::State)?
                .map(|v| <Self::State as From<u8>>::from(v))
                .unwrap_or_default();

            let state = self
                .handle_event(state, event)
                .await
                .map_err(HandleEventError::Saga)?;

            if let Some(state) = state {
                storage
                    .persist_state(id, state.into())
                    .await
                    .map_err(HandleEventError::State)?;
            } else {
                storage
                    .delete_state(id)
                    .await
                    .map_err(HandleEventError::State)?;
            }
        }
        Ok(())
    }
}
