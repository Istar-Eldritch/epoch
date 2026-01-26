//! Integration tests for transactional aggregate support.
//!
//! These tests verify that the transaction support works correctly with PostgreSQL,
//! including atomicity, isolation, and performance benefits.

mod common;

use async_trait::async_trait;
use epoch_core::aggregate::{
    Aggregate, AggregateState, AggregateTransaction, Command, HandleCommandError,
    TransactionalAggregate,
};
use epoch_core::event::{Event, EventData};
use epoch_core::event_applicator::{EventApplicator, EventApplicatorState};
use epoch_core::prelude::{EventBus, EventStoreBackend, StateStoreBackend};
use epoch_derive::EventData;
use epoch_mem::InMemoryEventBus;
use epoch_pg::Migrator;
use epoch_pg::aggregate::{PgAggregateError, PgTransaction};
use epoch_pg::event_store::PgEventStore;
use epoch_pg::state_store::{PgState, PgStateStore};
use serde::{Deserialize, Serialize};
use serial_test::serial;
use sqlx::{FromRow, PgExecutor, PgPool};
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

// =============================================================================
// Test Domain Types
// =============================================================================

/// Test event data for the counter aggregate
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, EventData)]
enum CounterEvent {
    Created { id: Uuid, initial_value: i64 },
    Incremented { id: Uuid, amount: i64 },
    Decremented { id: Uuid, amount: i64 },
}

// Required for EventApplicator - converts from reference to owned
impl TryFrom<&CounterEvent> for CounterEvent {
    type Error = epoch_core::event::EnumConversionError;

    fn try_from(value: &CounterEvent) -> Result<Self, Self::Error> {
        Ok(value.clone())
    }
}

/// Test command data for the counter aggregate
#[derive(Debug, Clone)]
enum CounterCommand {
    Create { id: Uuid, initial_value: i64 },
    Increment { id: Uuid, amount: i64 },
    Decrement { id: Uuid, amount: i64 },
}

/// Test state for the counter aggregate
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
struct CounterState {
    id: Uuid,
    value: i64,
    version: i64,
}

impl EventApplicatorState for CounterState {
    fn get_id(&self) -> &Uuid {
        &self.id
    }
}

impl AggregateState for CounterState {
    fn get_version(&self) -> u64 {
        self.version as u64
    }

    fn set_version(&mut self, version: u64) {
        self.version = version as i64;
    }
}

#[async_trait]
impl PgState for CounterState {
    async fn find_by_id<'a, T>(id: Uuid, executor: T) -> Result<Option<Self>, sqlx::Error>
    where
        T: PgExecutor<'a>,
    {
        sqlx::query_as("SELECT id, value, version FROM counter_states WHERE id = $1")
            .bind(id)
            .fetch_optional(executor)
            .await
    }

    async fn find_by_id_for_update<'a, T>(
        id: Uuid,
        executor: T,
    ) -> Result<Option<Self>, sqlx::Error>
    where
        T: PgExecutor<'a>,
    {
        sqlx::query_as("SELECT id, value, version FROM counter_states WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(executor)
            .await
    }

    async fn upsert<'a, T>(id: Uuid, state: &'a Self, executor: T) -> Result<(), sqlx::Error>
    where
        T: PgExecutor<'a>,
    {
        sqlx::query(
            "INSERT INTO counter_states (id, value, version) VALUES ($1, $2, $3) \
             ON CONFLICT (id) DO UPDATE SET value = $2, version = $3",
        )
        .bind(id)
        .bind(state.value)
        .bind(state.version)
        .execute(executor)
        .await?;
        Ok(())
    }

    async fn delete<'a, T>(id: Uuid, executor: T) -> Result<(), sqlx::Error>
    where
        T: PgExecutor<'a>,
    {
        sqlx::query("DELETE FROM counter_states WHERE id = $1")
            .bind(id)
            .execute(executor)
            .await?;
        Ok(())
    }
}

// =============================================================================
// Test Aggregate Implementation
// =============================================================================

/// Error type for counter aggregate
#[derive(Debug, thiserror::Error)]
enum CounterError {
    #[error("Counter already exists")]
    AlreadyExists,
    #[error("Counter not found")]
    NotFound,
    #[error("Cannot decrement below zero")]
    BelowZero,
}

/// The counter aggregate for testing transactions
struct CounterAggregate {
    pool: PgPool,
    event_store: PgEventStore<InMemoryEventBus<CounterEvent>>,
    state_store: PgStateStore<CounterState>,
}

impl CounterAggregate {
    fn new(pool: PgPool, event_store: PgEventStore<InMemoryEventBus<CounterEvent>>) -> Self {
        Self {
            pool: pool.clone(),
            event_store,
            state_store: PgStateStore::new(pool),
        }
    }
}

impl EventApplicator<CounterEvent> for CounterAggregate {
    type State = CounterState;
    type StateStore = PgStateStore<CounterState>;
    type EventType = CounterEvent;
    type ApplyError = std::convert::Infallible;

    fn get_state_store(&self) -> Self::StateStore {
        self.state_store.clone()
    }

    fn apply(
        &self,
        state: Option<Self::State>,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::ApplyError> {
        match &event.data {
            Some(CounterEvent::Created { id, initial_value }) => Ok(Some(CounterState {
                id: *id,
                value: *initial_value,
                version: 0,
            })),
            Some(CounterEvent::Incremented { amount, .. }) => {
                let mut state = state.expect("State must exist for increment");
                state.value += amount;
                Ok(Some(state))
            }
            Some(CounterEvent::Decremented { amount, .. }) => {
                let mut state = state.expect("State must exist for decrement");
                state.value -= amount;
                Ok(Some(state))
            }
            None => Ok(state),
        }
    }
}

#[async_trait]
impl Aggregate<CounterEvent> for CounterAggregate {
    type CommandData = CounterCommand;
    type CommandCredentials = ();
    type Command = CounterCommand;
    type EventStore = PgEventStore<InMemoryEventBus<CounterEvent>>;
    type AggregateError = CounterError;

    fn get_event_store(&self) -> Self::EventStore {
        self.event_store.clone()
    }

    async fn handle_command(
        &self,
        state: &Option<Self::State>,
        command: Command<Self::Command, Self::CommandCredentials>,
    ) -> Result<Vec<Event<CounterEvent>>, Self::AggregateError> {
        match command.data {
            CounterCommand::Create { id, initial_value } => {
                if state.is_some() {
                    return Err(CounterError::AlreadyExists);
                }
                Ok(vec![
                    Event::<CounterEvent>::builder()
                        .stream_id(id)
                        .event_type("Created".to_string())
                        .data(Some(CounterEvent::Created { id, initial_value }))
                        .build()
                        .unwrap(),
                ])
            }
            CounterCommand::Increment { id, amount } => {
                if state.is_none() {
                    return Err(CounterError::NotFound);
                }
                Ok(vec![
                    Event::<CounterEvent>::builder()
                        .stream_id(id)
                        .event_type("Incremented".to_string())
                        .data(Some(CounterEvent::Incremented { id, amount }))
                        .build()
                        .unwrap(),
                ])
            }
            CounterCommand::Decrement { id, amount } => {
                let s = state.as_ref().ok_or(CounterError::NotFound)?;
                if s.value - amount < 0 {
                    return Err(CounterError::BelowZero);
                }
                Ok(vec![
                    Event::<CounterEvent>::builder()
                        .stream_id(id)
                        .event_type("Decremented".to_string())
                        .data(Some(CounterEvent::Decremented { id, amount }))
                        .build()
                        .unwrap(),
                ])
            }
        }
    }
}

// Note: TryFrom<CounterCommand> for CounterCommand is auto-implemented via blanket impl
// since CounterCommand: Into<CounterCommand> (reflexive)

// =============================================================================
// TransactionalAggregate Implementation
// =============================================================================

/// Type alias for the transaction error
type CounterTransactionError =
    PgAggregateError<<InMemoryEventBus<CounterEvent> as EventBus>::Error>;

#[async_trait]
impl TransactionalAggregate for CounterAggregate {
    type SupersetEvent = CounterEvent;
    type Transaction = PgTransaction<CounterTransactionError>;
    type TransactionError = CounterTransactionError;

    async fn begin(
        self: Arc<Self>,
    ) -> Result<AggregateTransaction<Self, Self::Transaction>, Self::TransactionError> {
        let tx = self.pool.begin().await?;
        Ok(AggregateTransaction::new(self, PgTransaction::new(tx)))
    }

    async fn store_events_in_tx(
        &self,
        tx: &mut Self::Transaction,
        events: Vec<Event<Self::SupersetEvent>>,
    ) -> Result<Vec<Event<Self::SupersetEvent>>, Self::TransactionError> {
        self.event_store
            .store_events_in_tx(&mut *tx, events)
            .await
            .map_err(PgAggregateError::EventStore)
    }

    async fn get_state_in_tx(
        &self,
        tx: &mut Self::Transaction,
        id: Uuid,
    ) -> Result<Option<Self::State>, Self::TransactionError> {
        CounterState::find_by_id_for_update(id, tx.as_mut())
            .await
            .map_err(Into::into)
    }

    async fn persist_state_in_tx(
        &self,
        tx: &mut Self::Transaction,
        id: Uuid,
        state: Self::State,
    ) -> Result<(), Self::TransactionError> {
        CounterState::upsert(id, &state, tx.as_mut())
            .await
            .map_err(Into::into)
    }

    async fn delete_state_in_tx(
        &self,
        tx: &mut Self::Transaction,
        id: Uuid,
    ) -> Result<(), Self::TransactionError> {
        CounterState::delete(id, tx.as_mut())
            .await
            .map_err(Into::into)
    }

    async fn publish_event(
        &self,
        event: Event<Self::SupersetEvent>,
    ) -> Result<(), Self::TransactionError> {
        self.event_store
            .publish_events(vec![event])
            .await
            .map_err(PgAggregateError::EventStore)
    }
}

// =============================================================================
// Test Setup
// =============================================================================

async fn setup() -> (PgPool, Arc<CounterAggregate>) {
    let pool = common::get_pg_pool().await;

    // Run epoch migrations
    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

    // Create counter_states table
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS counter_states (
            id UUID PRIMARY KEY,
            value BIGINT NOT NULL,
            version BIGINT NOT NULL
        )",
    )
    .execute(&pool)
    .await
    .expect("Failed to create counter_states table");

    let event_bus = InMemoryEventBus::new();
    let event_store = PgEventStore::new(pool.clone(), event_bus);
    let aggregate = Arc::new(CounterAggregate::new(pool.clone(), event_store));

    (pool, aggregate)
}

async fn teardown(pool: &PgPool) {
    sqlx::query("DROP TABLE IF EXISTS counter_states CASCADE")
        .execute(pool)
        .await
        .expect("Failed to drop counter_states table");
}

// =============================================================================
// Tests
// =============================================================================

#[tokio::test]
#[serial]
async fn test_transaction_commit_persists_events_and_state() {
    let (pool, aggregate) = setup().await;

    let id = Uuid::new_v4();

    // Begin transaction and handle command
    let mut tx = aggregate.clone().begin().await.unwrap();
    let state = tx
        .handle(Command::new(
            id,
            CounterCommand::Create {
                id,
                initial_value: 100,
            },
            None,
            None,
        ))
        .await
        .unwrap();

    // State should be returned
    assert!(state.is_some());
    let state = state.unwrap();
    assert_eq!(state.id, id);
    assert_eq!(state.value, 100);
    assert_eq!(state.version, 1);

    // Before commit: state should NOT be visible from another connection
    let state_before = aggregate.state_store.get_state(id).await.unwrap();
    assert!(
        state_before.is_none(),
        "State should not be visible before commit"
    );

    // Commit
    tx.commit().await.unwrap();

    // After commit: state should be visible
    let state_after = aggregate.state_store.get_state(id).await.unwrap();
    assert!(
        state_after.is_some(),
        "State should be visible after commit"
    );
    assert_eq!(state_after.unwrap().value, 100);

    // Events should be in the store
    use futures_util::StreamExt;
    let mut events = aggregate.event_store.read_events(id).await.unwrap();
    let event = events.next().await.unwrap().unwrap();
    assert!(matches!(
        event.data,
        Some(CounterEvent::Created {
            initial_value: 100,
            ..
        })
    ));

    teardown(&pool).await;
}

#[tokio::test]
#[serial]
async fn test_transaction_rollback_discards_changes() {
    let (pool, aggregate) = setup().await;

    let id = Uuid::new_v4();

    // Begin transaction and handle command
    let mut tx = aggregate.clone().begin().await.unwrap();
    tx.handle(Command::new(
        id,
        CounterCommand::Create {
            id,
            initial_value: 100,
        },
        None,
        None,
    ))
    .await
    .unwrap();

    // Rollback instead of commit
    tx.rollback().await.unwrap();

    // State should NOT be visible
    let state = aggregate.state_store.get_state(id).await.unwrap();
    assert!(state.is_none(), "State should not exist after rollback");

    // Events should NOT be in the store
    use futures_util::StreamExt;
    let mut events = aggregate.event_store.read_events(id).await.unwrap();
    assert!(
        events.next().await.is_none(),
        "Events should not exist after rollback"
    );

    teardown(&pool).await;
}

#[tokio::test]
#[serial]
async fn test_transaction_multiple_handles_version_continuity() {
    let (pool, aggregate) = setup().await;

    let id = Uuid::new_v4();

    // Begin transaction and handle multiple commands
    let mut tx = aggregate.clone().begin().await.unwrap();

    // Create counter
    let state = tx
        .handle(Command::new(
            id,
            CounterCommand::Create {
                id,
                initial_value: 0,
            },
            None,
            None,
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.version, 1);
    assert_eq!(state.value, 0);

    // Increment (should use cached state with version 1)
    let state = tx
        .handle(Command::new(
            id,
            CounterCommand::Increment { id, amount: 10 },
            None,
            None,
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.version, 2);
    assert_eq!(state.value, 10);

    // Increment again (should use cached state with version 2)
    let state = tx
        .handle(Command::new(
            id,
            CounterCommand::Increment { id, amount: 5 },
            None,
            None,
        ))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.version, 3);
    assert_eq!(state.value, 15);

    // Commit
    tx.commit().await.unwrap();

    // Verify final state
    let final_state = aggregate.state_store.get_state(id).await.unwrap().unwrap();
    assert_eq!(final_state.version, 3);
    assert_eq!(final_state.value, 15);

    // Verify all 3 events were stored
    use futures_util::StreamExt;
    let mut events = aggregate.event_store.read_events(id).await.unwrap();
    let mut count = 0;
    while events.next().await.is_some() {
        count += 1;
    }
    assert_eq!(count, 3);

    teardown(&pool).await;
}

#[tokio::test]
#[serial]
async fn test_transaction_error_during_handle_allows_rollback() {
    let (pool, aggregate) = setup().await;

    let id = Uuid::new_v4();

    // Begin transaction
    let mut tx = aggregate.clone().begin().await.unwrap();

    // Create counter with value 5
    tx.handle(Command::new(
        id,
        CounterCommand::Create {
            id,
            initial_value: 5,
        },
        None,
        None,
    ))
    .await
    .unwrap();

    // Try to decrement by 10 (should fail - below zero)
    let result = tx
        .handle(Command::new(
            id,
            CounterCommand::Decrement { id, amount: 10 },
            None,
            None,
        ))
        .await;

    assert!(matches!(
        result,
        Err(HandleCommandError::Command(CounterError::BelowZero))
    ));

    // Rollback the entire transaction (including the successful create)
    tx.rollback().await.unwrap();

    // Nothing should be persisted
    let state = aggregate.state_store.get_state(id).await.unwrap();
    assert!(state.is_none());

    teardown(&pool).await;
}

/// Tests that batched transactions provide significant performance improvement.
///
/// This test compares individual handle() calls vs a single batched transaction
/// for creating multiple aggregates. The batched approach should be at least
/// 2x faster due to reduced round-trips and single fsync.
#[tokio::test]
#[serial]
async fn test_batch_seeding_performance() {
    let (pool, aggregate) = setup().await;

    let num_counters = 100;

    // Warmup phase to reduce variance from cold caches
    for _ in 0..10 {
        let id = Uuid::new_v4();
        aggregate
            .handle(Command::new(
                id,
                CounterCommand::Create {
                    id,
                    initial_value: 0,
                },
                None,
                None,
            ))
            .await
            .unwrap();
    }
    // Clean up warmup data
    sqlx::query("DELETE FROM counter_states")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM epoch_events")
        .execute(&pool)
        .await
        .unwrap();

    // Method 1: Individual handles (baseline)
    let start_individual = Instant::now();
    for i in 0..num_counters {
        let id = Uuid::new_v4();
        aggregate
            .handle(Command::new(
                id,
                CounterCommand::Create {
                    id,
                    initial_value: i as i64,
                },
                None,
                None,
            ))
            .await
            .unwrap();
    }
    let duration_individual = start_individual.elapsed();

    // Clean up both tables for fair comparison
    sqlx::query("DELETE FROM counter_states")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM epoch_events")
        .execute(&pool)
        .await
        .unwrap();

    // Method 2: Batched transaction
    // Store IDs to verify correctness
    let mut batched_ids: Vec<(Uuid, i64)> = Vec::with_capacity(num_counters as usize);
    let start_batched = Instant::now();
    let mut tx = aggregate.clone().begin().await.unwrap();
    for i in 0..num_counters {
        let id = Uuid::new_v4();
        batched_ids.push((id, i as i64));
        tx.handle(Command::new(
            id,
            CounterCommand::Create {
                id,
                initial_value: i as i64,
            },
            None,
            None,
        ))
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    let duration_batched = start_batched.elapsed();

    let speedup = duration_individual.as_secs_f64() / duration_batched.as_secs_f64();
    println!(
        "Individual: {:?}, Batched: {:?}, Speedup: {:.2}x",
        duration_individual, duration_batched, speedup
    );

    // Performance regression check: batched should be at least 2x faster
    // This threshold is conservative to avoid flaky tests on slow CI systems
    assert!(
        speedup > 2.0,
        "Batched transactions should be at least 2x faster than individual handles. \
         Got only {:.2}x speedup. This may indicate a performance regression.",
        speedup
    );

    // Verify count
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM counter_states")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count.0, num_counters as i64);

    // Verify correctness - each state should have the expected initial value
    for (id, expected_value) in batched_ids {
        let state = aggregate.state_store.get_state(id).await.unwrap();
        assert!(state.is_some(), "State for {} should exist", id);
        assert_eq!(
            state.unwrap().value,
            expected_value,
            "State {} has wrong value",
            id
        );
    }

    teardown(&pool).await;
}

/// Tests that concurrent transactions are properly isolated via row-level locking.
///
/// This test verifies that when two transactions try to modify the same aggregate,
/// the second transaction waits for the first to complete (FOR UPDATE semantics).
#[tokio::test]
#[serial]
async fn test_concurrent_transactions_isolation() {
    use tokio::sync::Barrier;

    let (pool, aggregate) = setup().await;

    let id = Uuid::new_v4();

    // First, create the counter outside any transaction
    aggregate
        .handle(Command::new(
            id,
            CounterCommand::Create {
                id,
                initial_value: 0,
            },
            None,
            None,
        ))
        .await
        .unwrap();

    // Use a barrier to ensure both tasks are ready before proceeding
    let barrier = Arc::new(Barrier::new(2));

    let aggregate1 = aggregate.clone();
    let aggregate2 = aggregate.clone();
    let barrier1 = barrier.clone();
    let barrier2 = barrier.clone();

    // Transaction 1: increment by 10
    let handle1 = tokio::spawn(async move {
        let mut tx = aggregate1.clone().begin().await.unwrap();

        // Get state (acquires FOR UPDATE lock)
        tx.handle(Command::new(
            id,
            CounterCommand::Increment { id, amount: 10 },
            None,
            None,
        ))
        .await
        .unwrap();

        // Signal that we have the lock, wait for tx2 to be ready
        barrier1.wait().await;

        // Hold the lock briefly to ensure tx2 has to wait
        tokio::time::sleep(Duration::from_millis(50)).await;

        tx.commit().await.unwrap();
    });

    // Transaction 2: increment by 5 (will wait for tx1 to release lock)
    let handle2 = tokio::spawn(async move {
        // Wait for tx1 to acquire the lock first
        barrier2.wait().await;

        let mut tx = aggregate2.clone().begin().await.unwrap();

        // This will block until tx1 commits (due to FOR UPDATE)
        tx.handle(Command::new(
            id,
            CounterCommand::Increment { id, amount: 5 },
            None,
            None,
        ))
        .await
        .unwrap();

        tx.commit().await.unwrap();
    });

    // Wait for both to complete
    handle1.await.unwrap();
    handle2.await.unwrap();

    // Final value should be 15 (0 + 10 + 5)
    let final_state = aggregate.state_store.get_state(id).await.unwrap().unwrap();
    assert_eq!(final_state.value, 15);

    // Verify version numbers are correct (create=1, increment=2, increment=3)
    assert_eq!(final_state.version, 3, "Version should be 3 after 3 events");

    teardown(&pool).await;
}

#[tokio::test]
#[serial]
async fn test_drop_without_commit_is_implicit_rollback() {
    let (pool, aggregate) = setup().await;

    let id = Uuid::new_v4();

    // Begin transaction, handle command, but don't commit or rollback
    {
        let mut tx = aggregate.clone().begin().await.unwrap();
        tx.handle(Command::new(
            id,
            CounterCommand::Create {
                id,
                initial_value: 100,
            },
            None,
            None,
        ))
        .await
        .unwrap();
        // tx is dropped here without commit
    }

    // State should NOT be visible (implicit rollback)
    let state = aggregate.state_store.get_state(id).await.unwrap();
    assert!(
        state.is_none(),
        "State should not exist after implicit rollback"
    );

    teardown(&pool).await;
}

#[tokio::test]
#[serial]
async fn test_pending_event_count() {
    let (pool, aggregate) = setup().await;

    let id = Uuid::new_v4();

    let mut tx = aggregate.clone().begin().await.unwrap();
    assert_eq!(tx.pending_event_count(), 0);

    tx.handle(Command::new(
        id,
        CounterCommand::Create {
            id,
            initial_value: 0,
        },
        None,
        None,
    ))
    .await
    .unwrap();
    assert_eq!(tx.pending_event_count(), 1);

    tx.handle(Command::new(
        id,
        CounterCommand::Increment { id, amount: 10 },
        None,
        None,
    ))
    .await
    .unwrap();
    assert_eq!(tx.pending_event_count(), 2);

    tx.commit().await.unwrap();

    teardown(&pool).await;
}
