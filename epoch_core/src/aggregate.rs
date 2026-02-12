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
use std::collections::HashMap;
use std::sync::Arc;
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
    /// The ID of the event that caused this command to be dispatched.
    ///
    /// This is `None` for commands originating from external triggers (e.g., HTTP requests)
    /// and `Some(event_id)` when the command is dispatched by a saga reacting to an event.
    pub causation_id: Option<Uuid>,
    /// The correlation ID to propagate to events produced by this command.
    ///
    /// All events in a causal tree share the same correlation ID. If not explicitly set,
    /// the aggregate will auto-generate one from the first event's ID.
    pub correlation_id: Option<Uuid>,
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
            causation_id: None,
            correlation_id: None,
        }
    }

    /// Sets causation context from a triggering event.
    ///
    /// This is the primary way sagas thread causation context when dispatching commands.
    /// Sets `causation_id = Some(event.id)` and inherits the event's `correlation_id`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// async fn handle_event(&self, state: Self::State, event: &Event<Self::EventType>)
    ///     -> Result<Option<Self::State>, Self::SagaError>
    /// {
    ///     match &event.data {
    ///         OrderEvent::OrderPlaced { order_id, .. } => {
    ///             let cmd = Command::new(*order_id, ReserveItems { .. }, None, None)
    ///                 .caused_by(event);
    ///             self.command_dispatcher.dispatch(cmd).await?;
    ///         }
    ///     }
    ///     Ok(Some(state))
    /// }
    /// ```
    pub fn caused_by<ED: EventData>(mut self, event: &Event<ED>) -> Self {
        self.causation_id = Some(event.id);
        self.correlation_id = event.correlation_id;
        self
    }

    /// Explicitly sets a correlation ID.
    ///
    /// Use this at system entry points (e.g., HTTP handlers) to inject an external
    /// trace ID as the correlation ID for all downstream events.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // In an HTTP handler
    /// let trace_id = extract_trace_id(request);
    /// let cmd = Command::new(order_id, PlaceOrder { .. }, Some(user), None)
    ///     .with_correlation_id(trace_id);
    /// aggregate.handle(cmd).await?;
    /// ```
    pub fn with_correlation_id(mut self, correlation_id: Uuid) -> Self {
        self.correlation_id = Some(correlation_id);
        self
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
            causation_id: self.causation_id,
            correlation_id: self.correlation_id,
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
            causation_id: self.causation_id,
            correlation_id: self.correlation_id,
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

    /// The transaction has already been consumed (committed or rolled back).
    ///
    /// This error occurs when attempting to call `handle()` on a transaction
    /// that has already been committed or rolled back.
    #[error("Transaction already consumed (committed or rolled back)")]
    TransactionConsumed,
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

            // Extract causation fields before consuming command in the iterator
            let causation_id = command.causation_id;
            let explicit_correlation = command.correlation_id;

            // Generate events from command
            let mut events: Vec<Event<ED>> = self
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

            // Stamp causation fields on all events
            if let Some(first_event) = events.first_mut() {
                // Set causation_id if provided
                first_event.causation_id = causation_id;

                // Set correlation_id: use explicit or auto-generate from first event's id
                first_event.correlation_id = Some(explicit_correlation.unwrap_or(first_event.id));
            }

            // Propagate correlation_id and causation_id to remaining events
            if events.len() > 1 {
                let correlation_id = events[0].correlation_id;
                let causation_id = events[0].causation_id;

                for event in events.iter_mut().skip(1) {
                    event.correlation_id = correlation_id;
                    event.causation_id = causation_id;
                }
            }

            let event_stream = Box::pin(SliceRefEventStream::from(events.as_slice()));
            let state = self
                .re_hydrate_from_refs(state, event_stream)
                .await
                .map_err(HandleCommandError::Hydration)?
                .map(|mut s| {
                    s.set_version(new_state_version);
                    s
                });

            debug!(
                "Storing {} events of type {:?}",
                events.len(),
                std::any::type_name::<ED>()
            );
            self.get_event_store()
                .store_events(events)
                .await
                .map_err(HandleCommandError::Event)?;

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

// ============================================================================
// Transaction Support
// ============================================================================

/// Errors that can occur when committing a transaction.
#[derive(Debug, thiserror::Error)]
pub enum CommitError<T, P>
where
    T: std::error::Error,
    P: std::error::Error,
{
    /// The database transaction failed to commit.
    #[error("Transaction commit failed: {0}")]
    Transaction(T),

    /// Event publishing failed after commit.
    ///
    /// Note: The transaction has already committed, so events are durable.
    /// Projections can catch up by replaying from the event store.
    #[error("Event publishing failed after commit: {0}")]
    Publish(P),

    /// The transaction has already been consumed (committed or rolled back).
    #[error("Transaction already consumed (committed or rolled back)")]
    TransactionConsumed,
}

/// Errors that can occur when rolling back a transaction.
#[derive(Debug, thiserror::Error)]
pub enum RollbackError<T>
where
    T: std::error::Error,
{
    /// The database transaction failed to rollback.
    #[error("Transaction rollback failed: {0}")]
    Transaction(T),

    /// The transaction has already been consumed (committed or rolled back).
    #[error("Transaction already consumed (committed or rolled back)")]
    TransactionConsumed,
}

/// Operations that a transaction must support.
///
/// This trait abstracts over different transaction implementations
/// (e.g., `sqlx::Transaction`, in-memory transactions).
#[async_trait]
pub trait TransactionOps: Send {
    /// The error type for transaction operations.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Commits the transaction, making all changes durable.
    async fn commit(self) -> Result<(), Self::Error>;

    /// Rolls back the transaction, discarding all changes.
    async fn rollback(self) -> Result<(), Self::Error>;
}

/// Error type alias for `AggregateTransaction::handle()`.
///
/// Simplifies the complex nested error type in method signatures.
///
/// # Why Different Error Types?
///
/// This alias uses the **event store's error type** for hydration errors rather than
/// the transaction error type. This distinction exists because:
///
/// 1. **Re-hydration reads are outside the transaction**: When `handle()` needs to
///    re-hydrate state from events, it calls `read_events_since` on the event store
///    directly, not through the transaction. This is safe because the FOR UPDATE
///    lock prevents concurrent writes.
///
/// 2. **Error type locality**: The event store error (e.g., `sqlx::Error`) is the
///    natural error type for event stream operations. Converting to the transaction
///    error type would lose information and add unnecessary coupling.
///
/// 3. **Clear error provenance**: Users can distinguish between errors from the
///    event store (hydration) vs errors from the transaction (state persistence).
///
/// # Example Resolution
///
/// For a typical PostgreSQL aggregate with:
/// - `AggregateError = MyBusinessError`
/// - `ApplyError = MyApplyError`
/// - `EventStore = PgEventStore<MyBus>` (where `EventStore::Error = sqlx::Error`)
/// - `TransactionError = PgAggregateError<MyBusError>`
///
/// This type alias resolves to:
/// ```ignore
/// HandleCommandError<
///     MyBusinessError,
///     ReHydrateError<MyApplyError, EnumConversionError, sqlx::Error>,
///     PgAggregateError<MyBusError>,
///     PgAggregateError<MyBusError>,
/// >
/// ```
pub type HandleInTxError<A> = HandleCommandError<
    <A as Aggregate<<A as TransactionalAggregate>::SupersetEvent>>::AggregateError,
    ReHydrateError<
        <A as EventApplicator<<A as TransactionalAggregate>::SupersetEvent>>::ApplyError,
        EnumConversionError,
        <<A as Aggregate<<A as TransactionalAggregate>::SupersetEvent>>::EventStore as EventStoreBackend>::Error,
    >,
    <A as TransactionalAggregate>::TransactionError,
    <A as TransactionalAggregate>::TransactionError,
>;

/// An aggregate that supports transactional command handling.
///
/// Extends [`Aggregate`] with the ability to begin a transaction that wraps
/// multiple operations atomically. Events and state changes are committed
/// together, and events are only published after the transaction commits.
///
/// # Usage
///
/// ```ignore
/// // Single command with atomic events + state
/// let aggregate = Arc::new(my_aggregate);
/// let mut tx = aggregate.clone().begin().await?;
/// let state = tx.handle(command).await?;
/// tx.commit().await?;
///
/// // Batch multiple commands in one transaction (e.g., seeding)
/// let mut tx = aggregate.clone().begin().await?;
/// for command in commands {
///     tx.handle(command).await?;
/// }
/// tx.commit().await?;  // Single fsync for all commands
/// ```
///
/// # Why `Arc<Self>`?
///
/// The `begin()` method takes `self: Arc<Self>` rather than `&self` to allow
/// the transaction to be moved across await points without lifetime issues.
/// This is necessary because async code often needs owned values.
#[async_trait]
pub trait TransactionalAggregate: Aggregate<Self::SupersetEvent> + Sized + Send + Sync
where
    <Self as EventApplicator<Self::SupersetEvent>>::State: AggregateState,
{
    /// The superset event type.
    ///
    /// Must match the `ED` parameter from the `Aggregate<ED>` implementation.
    /// This is the enum containing all event variants for the application.
    type SupersetEvent: EventData + Send + Sync + 'static;

    /// The transaction type (e.g., `sqlx::Transaction<'static, Postgres>`).
    type Transaction: TransactionOps<Error = Self::TransactionError> + Send;

    /// Error type for transaction operations.
    type TransactionError: std::error::Error + Send + Sync + 'static;

    /// Begins a new transaction.
    ///
    /// Returns a wrapper that provides the same `handle()` API but executes
    /// all operations within a single database transaction.
    ///
    /// # Ownership
    ///
    /// Takes `Arc<Self>` to allow the transaction to be moved across await points
    /// without lifetime issues.
    async fn begin(
        self: Arc<Self>,
    ) -> Result<AggregateTransaction<Self, Self::Transaction>, Self::TransactionError>;

    /// Stores events within a transaction.
    ///
    /// Events are inserted but not published. Returns events with database-assigned
    /// fields populated (e.g., `global_sequence` for PostgreSQL).
    async fn store_events_in_tx(
        &self,
        tx: &mut Self::Transaction,
        events: Vec<Event<Self::SupersetEvent>>,
    ) -> Result<Vec<Event<Self::SupersetEvent>>, Self::TransactionError>;

    /// Retrieves state within a transaction.
    ///
    /// For PostgreSQL, this should use `SELECT ... FOR UPDATE` to acquire a row-level
    /// lock, preventing concurrent modifications to the same aggregate.
    async fn get_state_in_tx(
        &self,
        tx: &mut Self::Transaction,
        id: Uuid,
    ) -> Result<Option<<Self as EventApplicator<Self::SupersetEvent>>::State>, Self::TransactionError>;

    /// Persists state within a transaction.
    async fn persist_state_in_tx(
        &self,
        tx: &mut Self::Transaction,
        id: Uuid,
        state: <Self as EventApplicator<Self::SupersetEvent>>::State,
    ) -> Result<(), Self::TransactionError>;

    /// Deletes state within a transaction.
    async fn delete_state_in_tx(
        &self,
        tx: &mut Self::Transaction,
        id: Uuid,
    ) -> Result<(), Self::TransactionError>;

    /// Publishes an event to the event bus (called after commit).
    async fn publish_event(
        &self,
        event: Event<Self::SupersetEvent>,
    ) -> Result<(), Self::TransactionError>;
}

/// A transactional wrapper around an aggregate.
///
/// Created by calling [`begin()`](TransactionalAggregate::begin) on a transactional
/// aggregate. Provides the same `handle()` method but executes all operations within
/// a database transaction.
///
/// Events are buffered and only published after [`commit()`](Self::commit) succeeds.
///
/// # Ownership Model
///
/// Uses `Arc<A>` rather than `&'a A` to avoid lifetime complexity when the
/// transaction needs to outlive the scope where it was created (e.g., across
/// await points in async code).
///
/// # Example
///
/// ```ignore
/// let aggregate = Arc::new(my_aggregate);
/// let mut tx = aggregate.clone().begin().await?;
///
/// // Multiple handles in one transaction
/// tx.handle(create_user_cmd).await?;
/// tx.handle(update_user_cmd).await?;
///
/// // Commit makes everything durable and publishes events
/// tx.commit().await?;
/// ```
pub struct AggregateTransaction<A, Tx>
where
    A: TransactionalAggregate,
    <A as EventApplicator<A::SupersetEvent>>::State: AggregateState,
{
    aggregate: Arc<A>,
    transaction: Option<Tx>,
    pending_events: Vec<Event<A::SupersetEvent>>,
    /// Cached state from previous `handle()` calls in this transaction.
    ///
    /// Ensures read-your-writes semantics for version tracking across multiple
    /// commands on the same aggregate within a single transaction.
    ///
    /// **Note for batch operations**: This cache is unbounded. For transactions
    /// spanning many distinct aggregates (e.g., thousands of creates), memory
    /// usage scales with the number of unique aggregate IDs accessed. If this
    /// becomes a concern, consider splitting large batches into multiple
    /// transactions.
    state_cache: HashMap<Uuid, Option<<A as EventApplicator<A::SupersetEvent>>::State>>,
}

impl<A, Tx> AggregateTransaction<A, Tx>
where
    A: TransactionalAggregate<Transaction = Tx>,
    Tx: TransactionOps<Error = A::TransactionError> + Send,
    <A as EventApplicator<A::SupersetEvent>>::State: AggregateState + Clone,
    <A as Aggregate<A::SupersetEvent>>::CommandData: Send + Sync,
    <<A as Aggregate<A::SupersetEvent>>::Command as TryFrom<
        <A as Aggregate<A::SupersetEvent>>::CommandData,
    >>::Error: Send + Sync,
{
    /// Creates a new `AggregateTransaction`.
    ///
    /// This is typically called by [`TransactionalAggregate::begin()`].
    pub fn new(aggregate: Arc<A>, transaction: Tx) -> Self {
        Self {
            aggregate,
            transaction: Some(transaction),
            pending_events: Vec::new(),
            state_cache: HashMap::new(),
        }
    }

    /// Handles a command within this transaction.
    ///
    /// Events and state changes are written to the transaction but not yet committed.
    /// Call [`commit()`](Self::commit) to persist all changes atomically.
    ///
    /// # Version Continuity
    ///
    /// When multiple `handle()` calls occur on the same aggregate within a transaction,
    /// version numbers are tracked correctly via an internal state cache:
    ///
    /// ```ignore
    /// tx.handle(create_cmd).await?;  // -> event v1, state.version = 1
    /// tx.handle(update_cmd).await?;  // -> event v2, state.version = 2 (uses cached state)
    /// tx.handle(update_cmd).await?;  // -> event v3, state.version = 3 (uses cached state)
    /// ```
    pub async fn handle(
        &mut self,
        command: Command<
            <A as Aggregate<A::SupersetEvent>>::CommandData,
            <A as Aggregate<A::SupersetEvent>>::CommandCredentials,
        >,
    ) -> Result<Option<<A as EventApplicator<A::SupersetEvent>>::State>, HandleInTxError<A>> {
        let transaction = self
            .transaction
            .as_mut()
            .ok_or(HandleCommandError::TransactionConsumed)?;

        if let Ok(cmd) = command.to_subset_command() {
            debug!(
                "Handling command in transaction: {:?}",
                std::any::type_name::<<A as Aggregate<A::SupersetEvent>>::Command>()
            );

            let state_id = self.aggregate.get_id_from_command(&command);

            // First check our cache (for subsequent handles in same tx)
            // Then fall back to reading from transaction
            let state = if let Some(cached) = self.state_cache.get(&state_id) {
                debug!("Using cached state for aggregate {}", state_id);
                cached.clone()
            } else {
                debug!("Reading state from transaction for aggregate {}", state_id);
                self.aggregate
                    .get_state_in_tx(transaction, state_id)
                    .await
                    .map_err(HandleCommandError::State)?
            };

            // Version check against current state (cached or fresh)
            let state_version = state.as_ref().map(|s| s.get_version()).unwrap_or(0);
            if let Some(expected) = command.aggregate_version
                && state_version != expected
            {
                debug!(
                    "Version mismatch in transaction. Expected: {}, Found: {}",
                    expected, state_version
                );
                return Err(HandleCommandError::VersionMismatch {
                    expected,
                    found: state_version,
                });
            }

            // Re-hydrate if needed (only on first access, cache handles subsequent).
            // For cached state, re-hydration already happened in previous handle().
            //
            // Safety: This read happens OUTSIDE the transaction, but it's safe because
            // the FOR UPDATE lock on the state row prevents any concurrent transaction
            // from writing new events for this aggregate until we release the lock
            // (on commit/rollback). Thus, no new events can appear between get_state_in_tx
            // and this read.
            let state = if self.state_cache.contains_key(&state_id) {
                state // Already up-to-date from previous handle()
            } else if let Some(state) = state {
                // First access: re-hydrate from event store
                let event_store = self.aggregate.get_event_store();
                let stream = event_store
                    .read_events_since(state_id, state.get_version() + 1)
                    .await
                    .map_err(|e| HandleCommandError::Hydration(ReHydrateError::EventStream(e)))?;
                self.aggregate
                    .re_hydrate::<<<A as Aggregate<A::SupersetEvent>>::EventStore as EventStoreBackend>::Error>(Some(state), stream)
                    .await
                    .map_err(HandleCommandError::Hydration)?
            } else {
                None
            };

            // Calculate next version based on (possibly re-hydrated) state
            let base_version = state.as_ref().map(|s| s.get_version()).unwrap_or(0);
            let next_version = base_version + 1;

            // Extract causation fields before consuming command in the iterator
            let causation_id = command.causation_id;
            let explicit_correlation = command.correlation_id;

            // Handle command to produce events
            let mut events: Vec<Event<A::SupersetEvent>> = self
                .aggregate
                .handle_command(&state, cmd)
                .await
                .map_err(HandleCommandError::Command)?
                .into_iter()
                .enumerate()
                .map(|(i, mut e)| {
                    e.stream_version = next_version + i as u64;
                    e
                })
                .collect();

            // Stamp causation fields on all events
            if let Some(first_event) = events.first_mut() {
                first_event.causation_id = causation_id;
                first_event.correlation_id = Some(explicit_correlation.unwrap_or(first_event.id));
            }

            if events.len() > 1 {
                let correlation_id = events[0].correlation_id;
                let causation_id = events[0].causation_id;

                for event in events.iter_mut().skip(1) {
                    event.correlation_id = correlation_id;
                    event.causation_id = causation_id;
                }
            }

            // Track the final version after all events
            let final_version = events.last().map_or(base_version, |e| e.stream_version);

            // Apply events to state
            let event_stream = Box::pin(SliceRefEventStream::from(events.as_slice()));
            let state = self
                .aggregate
                .re_hydrate_from_refs(state, event_stream)
                .await
                .map_err(HandleCommandError::Hydration)?
                .map(|mut s| {
                    s.set_version(final_version);
                    s
                });

            // Store events in transaction (no publish yet)
            let stored_events = self
                .aggregate
                .store_events_in_tx(transaction, events)
                .await
                .map_err(HandleCommandError::Event)?;

            // Buffer events for publishing after commit
            self.pending_events.extend(stored_events);

            // Store state in transaction
            if let Some(ref state) = state {
                self.aggregate
                    .persist_state_in_tx(transaction, state_id, state.clone())
                    .await
                    .map_err(HandleCommandError::State)?;
            } else {
                self.aggregate
                    .delete_state_in_tx(transaction, state_id)
                    .await
                    .map_err(HandleCommandError::State)?;
            }

            // Cache state for subsequent handle() calls on same aggregate
            self.state_cache.insert(state_id, state.clone());

            Ok(state)
        } else {
            debug!("Command not handled by this aggregate (subset conversion failed)");
            Ok(None)
        }
    }

    /// Commits the transaction and publishes all buffered events.
    ///
    /// After this call, all events and state changes are durable.
    /// Events are published to the event bus in order.
    ///
    /// # Errors
    ///
    /// - `CommitError::TransactionConsumed` - The transaction was already committed or rolled back
    /// - `CommitError::Transaction` - The database transaction failed to commit
    /// - `CommitError::Publish` - Event publishing failed (but transaction is committed)
    pub async fn commit(
        mut self,
    ) -> Result<(), CommitError<A::TransactionError, A::TransactionError>> {
        let transaction = self
            .transaction
            .take()
            .ok_or(CommitError::TransactionConsumed)?;

        transaction
            .commit()
            .await
            .map_err(CommitError::Transaction)?;

        // Publish events after commit.
        //
        // Publish semantics:
        // - Events are published sequentially in order
        // - On first failure, remaining events are NOT published
        // - The transaction is already committed at this point, so data is durable
        // - Projections can catch up via replay from the event store
        //
        // We use fail-fast rather than continue-on-error because:
        // 1. Event ordering must be preserved - skipping failed events breaks ordering
        // 2. Partial publish is recoverable via replay, partial ordering is not
        // 3. The error signals to the caller that manual intervention may be needed
        let total_events = self.pending_events.len();
        for (i, event) in self.pending_events.into_iter().enumerate() {
            if let Err(e) = self.aggregate.publish_event(event).await {
                log::error!(
                    "Failed to publish event {}/{} after commit: {:?}",
                    i + 1,
                    total_events,
                    e
                );
                return Err(CommitError::Publish(e));
            }
        }

        Ok(())
    }

    /// Rolls back the transaction, discarding all changes.
    ///
    /// No events are published. State changes are discarded.
    ///
    /// # Errors
    ///
    /// - `RollbackError::TransactionConsumed` - The transaction was already committed or rolled back
    /// - `RollbackError::Transaction` - The database rollback failed
    pub async fn rollback(mut self) -> Result<(), RollbackError<A::TransactionError>> {
        let transaction = self
            .transaction
            .take()
            .ok_or(RollbackError::TransactionConsumed)?;

        transaction
            .rollback()
            .await
            .map_err(RollbackError::Transaction)
    }

    /// Returns a reference to the underlying aggregate.
    pub fn aggregate(&self) -> &A {
        &self.aggregate
    }

    /// Returns the number of pending events buffered for publishing.
    pub fn pending_event_count(&self) -> usize {
        self.pending_events.len()
    }
}
