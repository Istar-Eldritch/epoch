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

/// The command structure that wraps command data with metadata.
///
/// A command represents an intention to change state. It carries:
/// - The target aggregate ID
/// - The command-specific data
/// - Optional credentials for authorization
/// - Optional expected version for optimistic concurrency control
#[derive(Debug, Clone)]
pub struct Command<D, C> {
    /// The ID of the root aggregate affected by this command.
    pub aggregate_id: Uuid,
    /// The command-specific data.
    pub data: D,
    /// Optional credentials for authorization.
    pub credentials: Option<C>,
    /// Optional expected version of the aggregate for optimistic concurrency control.
    pub aggregate_version: Option<u64>,
}

/// Aggregate state with version tracking for optimistic concurrency.
///
/// Extends [`EventApplicatorState`] with version tracking, which is used to:
/// - Prevent conflicting concurrent modifications
/// - Track how many events have been applied to this aggregate
pub trait AggregateState: EventApplicatorState {
    /// Returns the current version of the aggregate state.
    ///
    /// The version is incremented with each applied event and is used for
    /// optimistic concurrency control to prevent conflicting updates.
    fn get_version(&self) -> u64;

    /// Sets the version of the aggregate state.
    ///
    /// Called by the aggregate after applying events to update the version.
    fn set_version(&mut self, version: u64);
}

impl<D, C> Command<D, C>
where
    D: std::fmt::Debug + Clone,
    C: std::fmt::Debug + Clone,
{
    /// Creates a new command with the given parameters.
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

    /// Transforms this command's data to a subset type.
    ///
    /// Used to convert from a general command type to a specific aggregate's command type.
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

    /// Transforms this command's data to a superset type.
    ///
    /// Used to convert from a specific aggregate's command type to a general command type.
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

/// Errors that can occur when handling a command.
#[derive(Debug, thiserror::Error)]
pub enum HandleCommandError<C, H, S, E> {
    /// The version of the state doesn't match the expected version.
    ///
    /// This indicates a concurrent modification - another process modified
    /// the aggregate between when the command was created and when it was processed.
    #[error("Expected to find state with version {expected} but found {found}")]
    VersionMismatch {
        /// The version expected by the command.
        expected: u64,
        /// The actual version of the state.
        found: u64,
    },

    /// An error raised while handling the command.
    ///
    /// This wraps business logic errors from the aggregate's `handle_command` method.
    #[error("Error while handling command: {0}")]
    Command(C),

    /// An error raised while re-hydrating state from the event store.
    #[error("Error while hydrating events: {0}")]
    Hydration(H),

    /// An error raised by the state store.
    #[error("Error while storing state: {0}")]
    State(S),

    /// An error raised while storing events.
    #[error("Error while storing events: {0}")]
    Event(E),
}

/// An aggregate handles commands and produces events.
///
/// An `Aggregate` is the central concept in event sourcing. It:
/// - Receives commands (intentions to change state)
/// - Validates commands against current state
/// - Produces events (facts about what happened)
/// - Applies events to update its state
///
/// # Important
///
/// Aggregates should NOT be subscribed to the event bus as projections.
/// The `handle()` method already persists state before publishing events.
/// Subscribing an aggregate to the bus causes duplicate writes and race conditions.
///
/// Note: `Aggregate` extends [`EventApplicator`], NOT [`Projection`](crate::projection::Projection).
/// This is intentional to prevent wrapping aggregates in [`ProjectionHandler`](crate::projection::ProjectionHandler).
///
/// # Type Parameters
///
/// * `ED` - The superset event data type (e.g., `ApplicationEvent`)
///
/// # Example
///
/// ```ignore
/// use epoch_core::prelude::*;
///
/// struct UserAggregate {
///     state_store: InMemoryStateStore<User>,
///     event_store: InMemoryEventStore<InMemoryEventBus<ApplicationEvent>>,
/// }
///
/// impl EventApplicator<ApplicationEvent> for UserAggregate {
///     type State = User;
///     type StateStore = InMemoryStateStore<User>;
///     type EventType = UserEvent;
///     type ApplyError = UserApplyError;
///
///     fn apply(&self, state: Option<Self::State>, event: &Event<Self::EventType>)
///         -> Result<Option<Self::State>, Self::ApplyError> { /* ... */ }
///
///     fn get_state_store(&self) -> Self::StateStore { /* ... */ }
/// }
///
/// #[async_trait]
/// impl Aggregate<ApplicationEvent> for UserAggregate {
///     type CommandData = ApplicationCommand;
///     type CommandCredentials = ();
///     type Command = UserCommand;
///     type AggregateError = UserAggregateError;
///     type EventStore = InMemoryEventStore<InMemoryEventBus<ApplicationEvent>>;
///
///     fn get_event_store(&self) -> Self::EventStore { /* ... */ }
///
///     async fn handle_command(&self, state: &Option<Self::State>, command: Command<Self::Command, Self::CommandCredentials>)
///         -> Result<Vec<Event<ApplicationEvent>>, Self::AggregateError> { /* ... */ }
/// }
/// ```
#[async_trait]
pub trait Aggregate<ED>: EventApplicator<ED>
where
    ED: EventData + Send + Sync + 'static,
    <Self as EventApplicator<ED>>::State: AggregateState,
    Self::CommandData: Send + Sync,
    <Self::Command as TryFrom<Self::CommandData>>::Error: Send + Sync,
{
    /// The overarching type of command data that this aggregate can process.
    ///
    /// This is typically a superset enum containing all commands for the application.
    type CommandData: Clone + std::fmt::Debug;

    /// The type of credentials used for authorization.
    type CommandCredentials: Clone + std::fmt::Debug + Send;

    /// The specific command type used by this aggregate.
    ///
    /// Must be convertible from `CommandData` via `TryFrom`.
    type Command: TryFrom<Self::CommandData> + Send;

    /// The event store backend for persisting events.
    type EventStore: EventStoreBackend<EventType = ED> + Send + Sync + 'static;

    /// Error type for command handling.
    type AggregateError: std::error::Error + Send + Sync + 'static;

    /// Returns the event store for this aggregate.
    fn get_event_store(&self) -> Self::EventStore;

    /// Handles a specific command, producing events.
    ///
    /// This method contains the core business logic of the aggregate. It:
    /// - Validates the command against the current state
    /// - Determines which events should be generated
    /// - Returns the events (but does NOT persist them)
    ///
    /// # Arguments
    ///
    /// * `state` - The current state, or `None` if the aggregate doesn't exist
    /// * `command` - The command to handle
    ///
    /// # Returns
    ///
    /// A vector of events to be persisted, or an error if the command is invalid.
    async fn handle_command(
        &self,
        state: &Option<Self::State>,
        command: Command<Self::Command, Self::CommandCredentials>,
    ) -> Result<Vec<Event<ED>>, Self::AggregateError>;

    /// Handles a command by retrieving state, validating, generating events, and persisting.
    ///
    /// This method orchestrates the full command handling process:
    ///
    /// 1. Retrieves current state from the state store
    /// 2. Checks version for optimistic concurrency (if `aggregate_version` is provided)
    /// 3. Re-hydrates from event store to catch up on any missed events
    /// 4. Calls `handle_command` to generate events
    /// 5. Applies events to update state
    /// 6. Persists events to the event store
    /// 7. Persists state to the state store
    ///
    /// # Arguments
    ///
    /// * `command` - The command to handle
    ///
    /// # Returns
    ///
    /// The updated state after command handling, or an error.
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
                    .persist_state(*state.get_id(), state.clone())
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

    /// Extracts the aggregate ID from a command.
    ///
    /// By default, returns the command's `aggregate_id`. Override this method if
    /// your aggregate ID should be derived differently.
    fn get_id_from_command(
        &self,
        command: &Command<Self::CommandData, Self::CommandCredentials>,
    ) -> Uuid {
        command.aggregate_id
    }
}
