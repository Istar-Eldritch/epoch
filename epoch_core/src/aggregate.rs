//! Aggregate definition

use crate::event::{EnumConversionError, Event, EventData};
use crate::event_store::EventStoreBackend;
use crate::prelude::{
    Projection, ProjectionState, ReHydrateError, SliceRefEventStream, StateStoreBackend,
};
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

/// `AggregateState` represents the current state of an aggregate.
/// It encapsulates the current data derived from a sequence of events.
pub trait AggregateState: ProjectionState {
    /// Returns the current version of the projection state. The version is incremented by the aggregate with each applied event and is used for
    /// optimistic concurrency control to prevent conflicting updates.
    fn get_version(&self) -> u64;
    /// Sets the version of the projection state. The version is incremented by the aggregate with each applied event and is used for
    /// optimistic concurrency control to prevent conflicting updates.
    fn set_version(&mut self, version: u64);
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
            aggregate_version: self.aggregate_version,
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
            aggregate_version: self.aggregate_version,
        }
    }
}

/// Defines the errors that can happen when handling a command
#[derive(Debug, thiserror::Error)]
pub enum HandleCommandError<C, H, S, E> {
    /// The version of the state is different than expected
    #[error("Expected to find state with version {expected} but found {found}")]
    VersionMismatch {
        /// The version expected by the command
        expected: u64,
        /// The actual version of the state
        found: u64,
    },
    /// An error raised while handling the command
    #[error("Error while handling command: {0}")]
    Command(C),
    /// An error raised by the state store
    #[error("Error while hydrating events: {0}")]
    Hydration(H),
    /// An error raised by the state store
    #[error("Error while storing state: {0}")]
    State(S),
    /// An error raised by the state store
    #[error("Error while storing events: {0}")]
    Event(E),
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
    <Self as Projection<ED>>::State: AggregateState,
    Self::CommandData: Send + Sync,
    <Self::Command as TryFrom<Self::CommandData>>::Error: Send + Sync,
{
    /// The overarching type of `Command` data that this `Aggregate` can process.
    /// Commands are instructions to the aggregate to perform an action.
    type CommandData: Clone + std::fmt::Debug;
    /// The type of credentials used by the commands on this application.
    type CommandCredentials: Clone + std::fmt::Debug + Send;
    /// The specific command type used to create a new instance of the aggregate.
    /// This type must be convertible from the general `CommandData` type.
    type Command: TryFrom<Self::CommandData> + Send;
    /// The event store backend responsible for persisting and retrieving events for this aggregate.
    type EventStore: EventStoreBackend<EventType = ED> + Send + Sync + 'static;
    /// Error while handling commands
    type AggregateError: std::error::Error + Send + Sync + 'static;
    /// Returns an instance of the event store configured for this aggregate.
    ///
    /// This method provides the concrete `EventStoreBackend` implementation that the aggregate
    /// will use to store and retrieve events. It allows the aggregate to interact with the
    /// underlying event persistence mechanism.
    ///
    /// # Returns
    /// An instance of `Self::EventStore`, which is a type that implements `EventStoreBackend`.
    fn get_event_store(&self) -> Self::EventStore;

    /// Handles a specific command, applying it to the current aggregate state and producing new events.
    ///
    /// This method is responsible for the core business logic of the aggregate. It takes the current
    /// state of the aggregate (if it exists) and a command, and based on these, it determines
    /// which events should be generated. These events represent the changes that occurred due to
    /// the command and will be persisted to the event store.
    ///
    /// # Arguments
    /// * `state` - An `Option` containing a reference to the current `AggregateState` of the aggregate.
    ///             If `None`, it means the aggregate does not currently exist (e.g., for a creation command).
    /// * `command` - The `Command` to be handled, containing the command-specific data and credentials.
    ///
    /// # Returns
    /// A `Result` indicating success or failure. On success, it returns a `Vec` of `Event<ED>`,
    /// which are the events generated by the command. On failure, it returns a `Box<dyn std::error::Error + Send + Sync>`
    /// representing the error that occurred during command handling.
    async fn handle_command(
        &self,
        state: &Option<Self::State>,
        command: Command<Self::Command, Self::CommandCredentials>,
    ) -> Result<Vec<Event<ED>>, Self::AggregateError>;

    /// Handles a generic command by retrieving the aggregate's state, applying the command,
    /// and persisting the resulting events and updated state.
    ///
    /// This method orchestrates the command handling process, including state retrieval,
    /// optimistic concurrency control (if `aggregate_version` is provided in the command),
    /// event generation via `handle_command`, state re-hydration, and persistence of
    /// the new state and events.
    ///
    /// # Arguments
    /// * `command` - The `Command` to be handled, containing the command-specific data and credentials.
    ///
    /// # Returns
    /// A `Result` indicating success or failure. On success, it returns `Ok(())`.
    /// On failure, it returns a `Box<dyn std::error::Error + Send + Sync>` representing
    /// the error that occurred during the handling process.
    async fn handle(
        &self,
        command: Command<Self::CommandData, Self::CommandCredentials>,
    ) -> Result<
        Option<<Self as Projection<ED>>::State>,
        HandleCommandError<
            Self::AggregateError,
            ReHydrateError<
                <Self as Projection<ED>>::ProjectionError,
                EnumConversionError,
                <<Self as Aggregate<ED>>::EventStore as EventStoreBackend>::Error,
            >,
            <Self::StateStore as StateStoreBackend<Self::State>>::Error,
            <Self::EventStore as EventStoreBackend>::Error,
        >,
    > {
        if let Ok(cmd) = command.to_subset_command() {
            debug!(
                "Handling command: {:?}",
                std::any::type_name::<Self::Command>()
            );
            let mut state_store = self.get_state_store();
            let state_id = self.get_id_from_command(&command);
            debug!("Retrieving state for command. State ID: {:?}", state_id);
            let state = state_store
                .get_state(state_id)
                .await
                .map_err(HandleCommandError::State)?;

            let state_version = state.as_ref().map(|s| s.get_version()).unwrap_or(0);
            if let Some(expected_version) = command.aggregate_version {
                debug!("Checking expected version");
                if state_version != expected_version {
                    debug!(
                        "Version mismatch for update command. Expected: {}, Found: {}",
                        expected_version, state_version
                    );
                    return Err(HandleCommandError::VersionMismatch {
                        expected: expected_version,
                        found: state_version,
                    });
                }
            }

            // Re-hydrate from event store to catch up with any events that occurred
            // after the state was persisted. This ensures we have the latest state
            // and correct version before handling the command.
            let state = if let Some(state) = state {
                let event_store = self.get_event_store();
                let stream = event_store
                    .read_events_since(state_id, state_version + 1)
                    .await
                    .map_err(HandleCommandError::Event)?;
                self.re_hydrate::<<Self::EventStore as EventStoreBackend>::Error>(
                    Some(state),
                    stream,
                )
                .await
                .map_err(HandleCommandError::Hydration)?
            } else {
                None
            };

            // Calculate the next version based on the re-hydrated state
            let mut new_state_version = state.as_ref().map(|s| s.get_version()).unwrap_or(0) + 1;

            let events: Vec<Event<ED>> = self
                .handle_command(&state, cmd)
                .await
                .map_err(HandleCommandError::Command)?
                .into_iter()
                .enumerate()
                .map(|(i, mut e)| {
                    e.stream_version = new_state_version + i as u64;
                    new_state_version = e.stream_version;
                    e
                })
                .collect();

            let event_stream = Box::pin(SliceRefEventStream::from(events.as_slice()));
            let state = self
                .re_hydrate_from_refs(state, event_stream)
                .await
                .map_err(HandleCommandError::Hydration)?
                .map(|mut s| {
                    s.set_version(new_state_version);
                    s
                });

            for event in events.into_iter() {
                debug!(
                    "Storing event: {:?} - v{}",
                    std::any::type_name::<ED>(),
                    event.stream_version
                );
                self.get_event_store()
                    .store_event(event)
                    .await
                    .map_err(HandleCommandError::Event)?;
            }

            if let Some(state) = state {
                debug!(
                    "Persisting state for command. State ID: {:?}",
                    state.get_id()
                );
                state_store
                    .persist_state(state.get_id(), state.clone())
                    .await
                    .map_err(HandleCommandError::State)?;
                Ok(Some(state))
            } else {
                debug!("Deleting state for command. State ID: {:?}", state_id);
                state_store
                    .delete_state(state_id)
                    .await
                    .map_err(HandleCommandError::State)?;
                Ok(None)
            }
        } else {
            debug!("Command handling complete.");
            Ok(None)
        }
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
