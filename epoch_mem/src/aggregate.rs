//! In-memory aggregate with transaction support.
//!
//! This module provides transaction support for in-memory aggregates, primarily
//! useful for testing. It simulates database transaction semantics using a mutex
//! for isolation and buffered writes.
//!
//! # Example
//!
//! ```ignore
//! use epoch_mem::{InMemoryTransaction, InMemoryTransactionError};
//! use epoch_core::aggregate::TransactionOps;
//!
//! // Create a transaction
//! let tx = InMemoryTransaction::new(event_store, state_store, lock_guard);
//!
//! // Buffer changes
//! tx.buffer_state(id, Some(state));
//! tx.buffer_events(events);
//!
//! // Commit applies all changes atomically
//! tx.commit().await?;
//! ```

use crate::{
    InMemoryEventStore, InMemoryEventStoreBackendError, InMemoryStateStore, InMemoryStateStoreError,
};
use async_trait::async_trait;
use epoch_core::aggregate::TransactionOps;
use epoch_core::event::{Event, EventData};
use epoch_core::prelude::{EventBus, EventStoreBackend, StateStoreBackend};
use std::collections::HashMap;
use tokio::sync::OwnedMutexGuard;
use uuid::Uuid;

/// Errors for in-memory aggregate transactions.
#[derive(Debug, thiserror::Error)]
pub enum InMemoryTransactionError<BE>
where
    BE: std::error::Error,
{
    /// Event store error.
    #[error("Event store error: {0}")]
    EventStore(#[from] InMemoryEventStoreBackendError),

    /// State store error.
    #[error("State store error: {0}")]
    StateStore(#[from] InMemoryStateStoreError),

    /// Event publishing error.
    #[error("Event publish error: {0}")]
    Publish(BE),
}

/// Pending changes for an in-memory transaction.
///
/// Buffers all changes until commit, then applies them atomically.
/// Uses a mutex lock to simulate database-level isolation - only one
/// transaction can run at a time.
///
/// # Isolation Level
///
/// This provides "exclusive access" isolation - only one transaction
/// runs at a time. This is stricter than typical database isolation
/// but simpler to implement and sufficient for testing.
pub struct InMemoryTransaction<S, E, B>
where
    S: Clone + Send + Sync + std::fmt::Debug,
    E: EventData + Send + Sync,
    B: EventBus<EventType = E> + Clone,
{
    /// Pending state changes (id -> new state or None for delete)
    pending_states: HashMap<Uuid, Option<S>>,
    /// Pending events to store
    pending_events: Vec<Event<E>>,
    /// Reference to the event store (for commit)
    event_store: InMemoryEventStore<B>,
    /// Reference to the state store (for commit)
    state_store: InMemoryStateStore<S>,
    /// Lock guard held for transaction duration
    _lock: OwnedMutexGuard<()>,
    /// Whether the transaction has been consumed (committed or rolled back)
    consumed: bool,
}

impl<S, E, B> InMemoryTransaction<S, E, B>
where
    S: Clone + Send + Sync + std::fmt::Debug,
    E: EventData + Send + Sync,
    B: EventBus<EventType = E> + Clone + Send + Sync,
{
    /// Creates a new in-memory transaction.
    ///
    /// The transaction holds the lock for its entire duration, ensuring
    /// exclusive access to the stores.
    pub fn new(
        event_store: InMemoryEventStore<B>,
        state_store: InMemoryStateStore<S>,
        lock: OwnedMutexGuard<()>,
    ) -> Self {
        Self {
            pending_states: HashMap::new(),
            pending_events: Vec::new(),
            event_store,
            state_store,
            _lock: lock,
            consumed: false,
        }
    }

    /// Buffers a state change for later commit.
    ///
    /// Use `Some(state)` to upsert, `None` to delete.
    pub fn buffer_state(&mut self, id: Uuid, state: Option<S>) {
        self.pending_states.insert(id, state);
    }

    /// Returns the pending state for an ID, if any.
    ///
    /// This enables read-your-writes semantics within a transaction.
    pub fn get_pending_state(&self, id: &Uuid) -> Option<&Option<S>> {
        self.pending_states.get(id)
    }

    /// Buffers events for later commit.
    pub fn buffer_events(&mut self, events: Vec<Event<E>>) {
        self.pending_events.extend(events);
    }

    /// Returns a reference to the state store for reads.
    pub fn state_store(&self) -> &InMemoryStateStore<S> {
        &self.state_store
    }

    /// Returns a reference to the event store for reads.
    pub fn event_store(&self) -> &InMemoryEventStore<B> {
        &self.event_store
    }
}

#[async_trait]
impl<S, E, B> TransactionOps for InMemoryTransaction<S, E, B>
where
    S: Clone + Send + Sync + std::fmt::Debug,
    E: EventData + Send + Sync,
    B: EventBus<EventType = E> + Clone + Send + Sync,
    B::Error: Send + Sync + 'static,
{
    type Error = InMemoryTransactionError<B::Error>;

    /// Commits all pending changes atomically.
    ///
    /// Order of operations (matches PostgreSQL semantics):
    /// 1. Apply all pending state changes
    /// 2. Store all pending events
    /// 3. Publish events (happens during store_events for in-memory)
    /// 4. Release the lock
    ///
    /// # Note on Event Publishing
    ///
    /// The in-memory event store publishes events synchronously during `store_events()`,
    /// whereas PostgreSQL publishes after the transaction commits. Both ensure state
    /// changes are visible before events are published, maintaining read-your-writes
    /// consistency for projections.
    async fn commit(mut self) -> Result<(), Self::Error> {
        self.consumed = true;

        // Apply state changes first (matches PostgreSQL: state committed before events published)
        for (id, state) in self.pending_states.drain() {
            match state {
                Some(s) => {
                    self.state_store.clone().persist_state(id, s).await?;
                }
                None => {
                    self.state_store.clone().delete_state(id).await?;
                }
            }
        }

        // Store and publish events after state is persisted
        // Note: In-memory event store publishes events synchronously during store_events()
        if !self.pending_events.is_empty() {
            self.event_store
                .store_events(std::mem::take(&mut self.pending_events))
                .await?;
        }

        // Lock is released when self is dropped
        Ok(())
    }

    /// Rolls back the transaction, discarding all changes.
    ///
    /// Simply drops all pending changes without applying them.
    async fn rollback(mut self) -> Result<(), Self::Error> {
        self.consumed = true;
        // Simply drop self - no changes were applied
        // Lock is released when self is dropped
        Ok(())
    }
}

impl<S, E, B> Drop for InMemoryTransaction<S, E, B>
where
    S: Clone + Send + Sync + std::fmt::Debug,
    E: EventData + Send + Sync,
    B: EventBus<EventType = E> + Clone,
{
    fn drop(&mut self) {
        // Implicit rollback on drop is expected safety behavior (e.g., on panic/early return).
        // Using debug level since this is not an error, just informational.
        if !self.consumed {
            log::debug!(
                "InMemoryTransaction dropped without commit() or rollback(). \
                 Changes were discarded (implicit rollback)."
            );
        }
    }
}

// ============================================================================
// Helper Macro for TransactionalAggregate Implementation
// ============================================================================

/// Implements `TransactionalAggregate` for an in-memory aggregate.
///
/// This macro reduces the boilerplate of implementing `TransactionalAggregate`
/// for aggregates that use in-memory stores.
///
/// # Example
///
/// ```ignore
/// use epoch_mem::impl_inmemory_transactional_aggregate;
///
/// pub struct MyAggregate {
///     lock: Arc<Mutex<()>>,
///     event_store: InMemoryEventStore<InMemoryEventBus<MyEvent>>,
///     state_store: InMemoryStateStore<MyState>,
/// }
///
/// impl_inmemory_transactional_aggregate! {
///     aggregate: MyAggregate,
///     event: MyEvent,
///     bus: InMemoryEventBus<MyEvent>,
///     lock_field: lock,
///     event_store_field: event_store,
///     state_store_field: state_store,
/// }
/// ```
#[macro_export]
macro_rules! impl_inmemory_transactional_aggregate {
    (
        aggregate: $aggregate:ty,
        event: $event:ty,
        bus: $bus:ty,
        lock_field: $lock_field:ident,
        event_store_field: $event_store_field:ident,
        state_store_field: $state_store_field:ident$(,)?
    ) => {
        #[::async_trait::async_trait]
        impl ::epoch_core::aggregate::TransactionalAggregate for $aggregate {
            type SupersetEvent = $event;
            type Transaction = $crate::InMemoryTransaction<
                <Self as ::epoch_core::event_applicator::EventApplicator<$event>>::State,
                $event,
                $bus,
            >;
            type TransactionError = $crate::InMemoryTransactionError<
                <$bus as ::epoch_core::event_store::EventBus>::Error,
            >;

            async fn begin(
                self: ::std::sync::Arc<Self>,
            ) -> ::std::result::Result<
                ::epoch_core::aggregate::AggregateTransaction<Self, Self::Transaction>,
                Self::TransactionError,
            > {
                let lock = self.$lock_field.clone().lock_owned().await;
                let tx = $crate::InMemoryTransaction::new(
                    self.$event_store_field.clone(),
                    self.$state_store_field.clone(),
                    lock,
                );
                Ok(::epoch_core::aggregate::AggregateTransaction::new(self, tx))
            }

            async fn store_events_in_tx(
                &self,
                tx: &mut Self::Transaction,
                events: ::std::vec::Vec<::epoch_core::event::Event<Self::SupersetEvent>>,
            ) -> ::std::result::Result<
                ::std::vec::Vec<::epoch_core::event::Event<Self::SupersetEvent>>,
                Self::TransactionError,
            > {
                tx.buffer_events(events.clone());
                Ok(events)
            }

            async fn get_state_in_tx(
                &self,
                tx: &mut Self::Transaction,
                id: ::uuid::Uuid,
            ) -> ::std::result::Result<
                ::std::option::Option<
                    <Self as ::epoch_core::event_applicator::EventApplicator<$event>>::State,
                >,
                Self::TransactionError,
            > {
                // Check pending state first (read-your-writes)
                if let ::std::option::Option::Some(pending) = tx.get_pending_state(&id) {
                    return Ok(pending.clone());
                }

                // Fall back to state store
                use ::epoch_core::state_store::StateStoreBackend;
                tx.state_store().get_state(id).await.map_err(Into::into)
            }

            async fn persist_state_in_tx(
                &self,
                tx: &mut Self::Transaction,
                id: ::uuid::Uuid,
                state: <Self as ::epoch_core::event_applicator::EventApplicator<$event>>::State,
            ) -> ::std::result::Result<(), Self::TransactionError> {
                tx.buffer_state(id, ::std::option::Option::Some(state));
                Ok(())
            }

            async fn delete_state_in_tx(
                &self,
                tx: &mut Self::Transaction,
                id: ::uuid::Uuid,
            ) -> ::std::result::Result<(), Self::TransactionError> {
                tx.buffer_state(id, ::std::option::Option::None);
                Ok(())
            }

            async fn publish_event(
                &self,
                event: ::epoch_core::event::Event<Self::SupersetEvent>,
            ) -> ::std::result::Result<(), Self::TransactionError> {
                use ::epoch_core::event_store::EventBus;
                self.$event_store_field
                    .bus()
                    .publish(::std::sync::Arc::new(event))
                    .await
                    .map_err($crate::InMemoryTransactionError::Publish)
            }
        }
    };
}

/// Unit tests for `InMemoryTransaction`.
///
/// Note: These tests cover `InMemoryTransaction` in isolation. Full `TransactionalAggregate`
/// integration tests are in `epoch_pg/tests/transaction_integration_tests.rs`. The in-memory
/// implementation is primarily for testing and development, so behavioral parity with
/// PostgreSQL is verified through the PostgreSQL test suite.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::InMemoryEventBus;
    use epoch_core::event::EnumConversionError;
    use epoch_core::prelude::StateStoreBackend;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    struct TestEvent {
        value: String,
    }

    impl EventData for TestEvent {
        fn event_type(&self) -> &'static str {
            "TestEvent"
        }
    }

    impl TryFrom<&TestEvent> for TestEvent {
        type Error = EnumConversionError;
        fn try_from(value: &TestEvent) -> Result<Self, Self::Error> {
            Ok(value.clone())
        }
    }

    #[derive(Debug, Clone, PartialEq)]
    struct TestState {
        id: Uuid,
        value: String,
    }

    #[tokio::test]
    async fn transaction_commit_applies_state_changes() {
        let bus = InMemoryEventBus::<TestEvent>::new();
        let event_store = InMemoryEventStore::new(bus);
        let state_store = InMemoryStateStore::<TestState>::new();
        let lock = Arc::new(Mutex::new(()));

        let id = Uuid::new_v4();
        let state = TestState {
            id,
            value: "test".to_string(),
        };

        // Create and commit transaction
        {
            let guard = lock.clone().lock_owned().await;
            let mut tx = InMemoryTransaction::new(event_store.clone(), state_store.clone(), guard);
            tx.buffer_state(id, Some(state.clone()));
            tx.commit().await.unwrap();
        }

        // Verify state was persisted
        let retrieved = state_store.get_state(id).await.unwrap();
        assert_eq!(retrieved, Some(state));
    }

    #[tokio::test]
    async fn transaction_rollback_discards_changes() {
        let bus = InMemoryEventBus::<TestEvent>::new();
        let event_store = InMemoryEventStore::new(bus);
        let state_store = InMemoryStateStore::<TestState>::new();
        let lock = Arc::new(Mutex::new(()));

        let id = Uuid::new_v4();
        let state = TestState {
            id,
            value: "test".to_string(),
        };

        // Create and rollback transaction
        {
            let guard = lock.clone().lock_owned().await;
            let mut tx = InMemoryTransaction::new(event_store.clone(), state_store.clone(), guard);
            tx.buffer_state(id, Some(state));
            tx.rollback().await.unwrap();
        }

        // Verify state was NOT persisted
        let retrieved = state_store.get_state(id).await.unwrap();
        assert_eq!(retrieved, None);
    }

    #[tokio::test]
    async fn transaction_read_your_writes() {
        let bus = InMemoryEventBus::<TestEvent>::new();
        let event_store = InMemoryEventStore::new(bus);
        let state_store = InMemoryStateStore::<TestState>::new();
        let lock = Arc::new(Mutex::new(()));

        let id = Uuid::new_v4();
        let state = TestState {
            id,
            value: "buffered".to_string(),
        };

        let guard = lock.clone().lock_owned().await;
        let mut tx = InMemoryTransaction::new(event_store, state_store, guard);

        // Buffer a state change
        tx.buffer_state(id, Some(state.clone()));

        // Read-your-writes: should see the buffered state
        let pending = tx.get_pending_state(&id);
        assert_eq!(pending, Some(&Some(state)));
    }
}
