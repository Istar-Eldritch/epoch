//! Aggregate definition

use crate::event::{Event, EventData};
use crate::event_store::EventStoreBackend;
use crate::prelude::{Projection, ProjectionState, StateStoreBackend};
use async_trait::async_trait;
use log::debug;
use uuid::Uuid;

/// The command structure
#[derive(Debug, Clone)]
pub struct Command<D, C> {
    /// The id of the root aggregate affected by this command
    pub aggregate_id: Uuid,
    /// The data used by the command
    pub data: D,
    /// The credentials of the command
    pub credentials: Option<C>,
    /// The expected version of aggregate
    pub aggregate_version: Option<u64>,
}

impl<D, C> Command<D, C>
where
    D: std::fmt::Debug + Clone,
    C: std::fmt::Debug + Clone,
{
    /// Create a new Command
    pub fn new(
        aggregate_id: Uuid,
        data: D,
        credentials: Option<C>,
        aggregate_version: Option<u64>,
    ) -> Self {
        Command {
            aggregate_id,
            data,
            credentials,
            aggregate_version,
        }
    }

    /// Transforms this command
    pub fn to_subset_command<CD>(&self) -> Result<Command<CD, C>, CD::Error>
    where
        CD: TryFrom<D>,
        CD::Error: Send + Sync,
    {
        let data = CD::try_from(self.data.clone())?;
        Ok(Command {
            aggregate_id: self.aggregate_id,
            data,
            credentials: self.credentials.clone(),
            aggregate_version: self.aggregate_version.clone(),
        })
    }

    /// Transforms this command
    pub fn to_superset_command<CD>(&self) -> Command<CD, C>
    where
        CD: EventData,
        D: Into<CD>,
    {
        let data = self.data.clone().into();
        Command {
            aggregate_id: self.aggregate_id,
            data,
            credentials: self.credentials.clone(),
            aggregate_version: self.aggregate_version.clone(),
        }
    }
}

/// Defines the errors that can happen when handling a command
#[derive(Debug, thiserror::Error)]
pub enum HandleCommandError {
    /// The state was not found so the command cannot be applied
    #[error("Could not find state for id {0}")]
    StateNotFound(Uuid),
    /// The version of the state is different than expected
    #[error("Expected to find state with version {expected} but found {found}")]
    VersionMismatch {
        /// The version expected by the command
        expected: u64,
        /// The actual version of the state
        found: u64,
    },
}

/// `Aggregate` is a central trait in `epoch_core` that defines the behavior of an aggregate in an event-sourced system.
/// An aggregate is a cluster of domain objects that can be treated as a single unit for data changes.
/// It is the consistency boundary for commands and events.
///
/// This trait provides the necessary associated types for the aggregate's state, commands, events,
/// and the stores used for persistence. It also defines the handlers for different types of commands
/// (create, update, delete) and a general command handler.
#[async_trait]
pub trait Aggregate<ED>: Projection<ED>
where
    ED: EventData + Send + Sync + 'static,
    Self::CommandData: Send + Sync,
    <Self::CreateCommand as TryFrom<Self::CommandData>>::Error: Send + Sync,
    <Self::UpdateCommand as TryFrom<Self::CommandData>>::Error: Send + Sync,
    <Self::DeleteCommand as TryFrom<Self::CommandData>>::Error: Send + Sync,
{
    /// The overarching type of `Command` that this `Aggregate` can process.
    /// Commands are instructions to the aggregate to perform an action.
    type CommandData: Clone + std::fmt::Debug;
    /// The type of credentials used by the commands on this application.
    type CommandCredentials: Clone + std::fmt::Debug + Send;
    /// The specific command type used to create a new instance of the aggregate.
    /// This type must be convertible from the general `Command` type.
    type CreateCommand: TryFrom<Self::CommandData> + Send;
    /// The specific command type used to update an existing instance of the aggregate.
    /// This type must be convertible from the general `Command` type.
    type UpdateCommand: TryFrom<Self::CommandData> + Send;
    /// The specific command type used to delete an existing instance of the aggregate.
    /// This type must be convertible from the general `Command` type.
    type DeleteCommand: TryFrom<Self::CommandData> + Send;
    /// The event store backend responsible for persisting and retrieving events for this aggregate.
    type EventStore: EventStoreBackend<EventType = ED> + Send + Sync + 'static;
    /// Returns an instance of the event store configured for this aggregate.
    fn get_event_store(&self) -> Self::EventStore;

    /// Handles a `CreateCommand` to create a new aggregate instance.
    ///
    /// This method is responsible for validating the command, applying business logic,
    /// and producing a new aggregate state along with a vector of events that represent
    /// the changes made.
    ///
    /// # Arguments
    /// * `command` - The `CreateCommand` to handle.
    ///
    /// # Returns
    /// A `Result` containing a tuple of the new `Self::State` and a `Vec<Event<Self::CreateEvent>>`
    /// on success, or a boxed `std::error::Error` on failure.
    async fn handle_create_command(
        &self,
        command: Command<Self::CreateCommand, Self::CommandCredentials>,
    ) -> Result<Vec<Event<ED>>, Box<dyn std::error::Error + Send + Sync>>;

    /// Handles an `UpdateCommand` to modify an existing aggregate instance.
    ///
    /// This method takes the current `state` of the aggregate, applies the `command`,
    /// and generates a new state and a vector of events reflecting the updates.
    ///
    /// # Arguments
    /// * `state` - The current `Self::State` of the aggregate.
    /// * `command` - The `UpdateCommand` to handle.
    ///
    /// # Returns
    /// A `Result` containing a tuple of the updated `Self::State` and a `Vec<Event<Self::UpdateEvent>>`
    /// on success, or a boxed `std::error::Error` on failure.
    async fn handle_update_command(
        &self,
        state: &Self::State,
        command: Command<Self::UpdateCommand, Self::CommandCredentials>,
    ) -> Result<Vec<Event<ED>>, Box<dyn std::error::Error + Send + Sync>>;

    /// Handles a `DeleteCommand` to mark an aggregate for deletion or to remove it.
    ///
    /// This method processes the `DeleteCommand` against the current `state`,
    /// and produces an optional new state (e.g., `None` if the aggregate is fully deleted,
    /// or `Some(State)` if it's logically deleted/archived) and a vector of events.
    ///
    /// # Arguments
    /// * `state` - The current `Self::State` of the aggregate.
    /// * `command` - The `DeleteCommand` to handle.
    ///
    /// # Returns
    /// A `Result` containing a tuple of an `Option<Self::State>` (indicating if a state remains)
    /// and a `Vec<Event<Self::DeleteEvent>>` on success, or a boxed `std::error::Error` on failure.
    async fn handle_delete_command(
        &self,
        state: &Self::State,
        command: Command<Self::DeleteCommand, Self::CommandCredentials>,
    ) -> Result<Vec<Event<ED>>, Box<dyn std::error::Error + Send + Sync>>;

    /// The general command handler for the aggregate.
    ///
    /// This method acts as a dispatcher, attempting to convert the incoming `Command`
    /// into a `CreateCommand`, `UpdateCommand`, or `DeleteCommand` and then delegating
    /// to the appropriate specific handler (`handle_create_command`, `handle_update_command`,
    /// or `handle_delete_command`).
    ///
    /// It also handles the persistence of generated events to the `EventStore` and
    /// the updated aggregate state to the `StateStore`. It includes optimistic concurrency
    /// control by checking the expected version for update and delete operations.
    ///
    /// # Arguments
    /// * `command` - The `Self::Command` to handle.
    ///
    /// # Errors
    /// Returns a `HandleCommandError::StateNotFound` if an update or delete command
    /// is issued for a state that does not exist.
    /// Returns a `HandleCommandError::VersionMismatch` if an update or delete command
    /// specifies an `expected_version` that does not match the current state's version,
    /// indicating a concurrency conflict.
    /// Returns other errors from the underlying event store, state store, or specific
    /// command handlers.
    async fn handle_command(
        &self,
        command: Command<Self::CommandData, Self::CommandCredentials>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        debug!(
            "Handling command: {:?}",
            std::any::type_name::<Self::CommandData>()
        );
        if let Ok(cmd) = command.to_subset_command() {
            debug!(
                "Handling create command: {:?}",
                std::any::type_name::<Self::CreateCommand>()
            );

            let events = self.handle_create_command(cmd).await?;

            let state = self.re_hydrate(None, events.iter())?;
            for event in events.into_iter() {
                debug!("Storing event: {:?}", std::any::type_name::<ED>());
                self.get_event_store().store_event(event).await?;
            }

            if let Some(state) = state {
                debug!(
                    "Persisting state for create command. State ID: {:?}",
                    state.get_id()
                );
                self.get_state_store()
                    .persist_state(state.get_id(), state)
                    .await?;
            } else {
                // TODO: Error when state was not aggregated from event
            }
        } else if let Ok(cmd) = command.to_subset_command() {
            debug!(
                "Handling update command: {:?}",
                std::any::type_name::<Self::UpdateCommand>()
            );
            let mut state_store = self.get_state_store();
            let state_id = self.get_id_from_command(&command);
            debug!(
                "Retrieving state for update command. State ID: {:?}",
                state_id
            );
            let state = state_store
                .get_state(state_id)
                .await?
                .ok_or(HandleCommandError::StateNotFound(state_id))?;

            if let Some(expected_version) = command.aggregate_version {
                if state.get_version() != expected_version {
                    debug!(
                        "Version mismatch for update command. Expected: {}, Found: {}",
                        expected_version,
                        state.get_version()
                    );
                    return Err(Box::new(HandleCommandError::VersionMismatch {
                        expected: expected_version,
                        found: state.get_version(),
                    }));
                }
            }
            let events = self.handle_update_command(&state, cmd).await?;

            let state = self.re_hydrate(Some(state), events.iter())?;

            for event in events.into_iter() {
                debug!("Storing event: {:?}", std::any::type_name::<ED>());
                self.get_event_store().store_event(event).await?;
            }

            if let Some(state) = state {
                debug!(
                    "Persisting state for update command. State ID: {:?}",
                    state.get_id()
                );
                state_store.persist_state(state.get_id(), state).await?;
            } else {
                // TODO: Error when state is not present after aggregation
            }
        } else if let Ok(cmd) = command.to_subset_command() {
            debug!(
                "Handling delete command: {:?}",
                std::any::type_name::<Self::DeleteCommand>()
            );
            let mut state_store = self.get_state_store();
            let state_id = self.get_id_from_command(&command);
            debug!(
                "Retrieving state for delete command. State ID: {:?}",
                state_id
            );
            let state = state_store
                .get_state(state_id)
                .await?
                .ok_or(HandleCommandError::StateNotFound(state_id))?;

            if let Some(expected_version) = command.aggregate_version {
                if state.get_version() != expected_version {
                    debug!(
                        "Version mismatch for delete command. Expected: {}, Found: {}",
                        expected_version,
                        state.get_version()
                    );
                    return Err(Box::new(HandleCommandError::VersionMismatch {
                        expected: expected_version,
                        found: state.get_version(),
                    }));
                }
            }

            let events = self.handle_delete_command(&state, cmd).await?;

            let state = self.re_hydrate(Some(state), events.iter())?;

            for event in events.into_iter() {
                debug!("Storing event: {:?}", std::any::type_name::<ED>());
                self.get_event_store().store_event(event).await?;
            }

            if let Some(state) = state {
                debug!(
                    "Persisting state for delete command. State ID: {:?}",
                    state.get_id()
                );
                state_store.persist_state(state.get_id(), state).await?;
            } else {
                debug!(
                    "Deleting state for delete command. State ID: {:?}",
                    state_id
                );
                state_store.delete_state(state_id).await?;
            }
        }
        debug!("Command handling complete.");
        Ok(())
    }

    /// Extracts and returns the unique identifier (UUID) of the aggregate instance
    /// from the given `command`.
    ///
    /// This is necessary because the generic `Command` type needs a way to provide
    /// the aggregate's ID for retrieval from the state store.
    ///
    /// # Arguments
    /// * `command` - A reference to the `Self::Command` from which to extract the ID.
    fn get_id_from_command(
        &self,
        command: &Command<Self::CommandData, Self::CommandCredentials>,
    ) -> Uuid {
        command.aggregate_id
    }
}
