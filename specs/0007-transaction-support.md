# Specification: Transaction Support

**Status:** Draft  
**Created:** 2026-01-26  
**Updated:** 2026-01-26  
**Author:** AI Agent  

---

## Revision History

| Date | Changes |
|------|---------|
| 2026-01-26 | Initial draft |
| 2026-01-26 | Addressed review comments: locking strategy, InMemoryTransaction design, version tracking, error types |
| 2026-01-26 | Simplified `pending_events` type, added `B` generic to `InMemoryTransaction`, preserved publish errors, added `HandleInTxError` type alias, documented why `handle()` doesn't use `begin()`/`commit()` internally, clarified lock ordering responsibility |

---

## 1. Problem Statement

The current `Aggregate::handle` implementation performs multiple individual database writes without transaction support:

```rust
// Each event is a separate write (separate fsync)
for event in events.into_iter() {
    self.get_event_store()
        .store_event(event)
        .await
        .map_err(HandleCommandError::Event)?;
}

// State is another separate write (another fsync)
state_store
    .persist_state(*state.get_id(), state.clone())
    .await
    .map_err(HandleCommandError::State)?;
```

This causes two problems:

### 1.1 Performance: fsync Per Write

Without transactions, PostgreSQL performs `fsync()` after each write. For a command producing 3 events + 1 state write = 4 fsyncs. For a seed script with ~10k writes = ~10k fsyncs = minutes instead of seconds.

### 1.2 Atomicity: Partial Writes on Failure

If the process crashes mid-command:
- Some events may be persisted, others not
- Events may be persisted but state not updated
- Leaves the system in an inconsistent state

The "events are source of truth, state can be rebuilt" argument is weak because:
- It requires manual intervention or additional tooling
- Re-hydration may be expensive for large event streams
- Users expect atomic command handling

## 2. Proposed Solution

Introduce a **transactional wrapper** for aggregates that provides the same `handle()` API but executes everything within a single database transaction.

### 2.1 Design Principles

1. **Same API** - `AggregateTransaction` has the same `handle()` method as `Aggregate`
2. **Explicit transactions** - User calls `begin()` to start, `commit()` to finish
3. **Atomic events + state** - Both written in the same transaction
4. **Publish after commit** - Events go to bus only after durable commit
5. **Batch support** - Multiple `handle()` calls can be batched in one transaction

### 2.2 Key Benefits

- **Atomicity**: Events and state are committed together or not at all
- **Performance**: Single fsync for multiple commands (great for seeding)
- **Familiar API**: Same `handle()` method, just wrapped in transaction lifecycle

## 3. API Design

### 3.1 Usage Example

```rust
// Single command with atomic events + state
let mut tx = aggregate.begin().await?;
let state = tx.handle(command).await?;
tx.commit().await?;

// Batch multiple commands in one transaction (e.g., seeding)
let mut tx = aggregate.begin().await?;
for command in commands {
    tx.handle(command).await?;
}
tx.commit().await?;  // Single fsync for all commands
```

### 3.2 AggregateTransaction Struct

```rust
/// Error type for `AggregateTransaction::handle()`.
///
/// Type alias to simplify the complex nested error type.
pub type HandleInTxError<A> = HandleCommandError<
    <A as Aggregate<<A as TransactionalAggregate>::SupersetEvent>>::AggregateError,
    ReHydrateError<
        <A as EventApplicator<<A as TransactionalAggregate>::SupersetEvent>>::ApplyError,
        EnumConversionError,
        <A as TransactionalAggregate>::TransactionError,
    >,
    <A as TransactionalAggregate>::TransactionError,
    <A as TransactionalAggregate>::TransactionError,
>;

/// A transactional wrapper around an aggregate.
///
/// Created by calling `begin()` on a transactional aggregate. Provides the same
/// `handle()` method but executes all operations within a database transaction.
///
/// Events are buffered and only published after `commit()` succeeds.
///
/// # Ownership Model
///
/// Uses `Arc<A>` rather than `&'a A` to avoid lifetime complexity when the
/// transaction needs to outlive the scope where it was created (e.g., across
/// await points in async code). The aggregate is shared, not borrowed.
pub struct AggregateTransaction<A, Tx>
where
    A: TransactionalAggregate,
{
    aggregate: Arc<A>,
    transaction: Tx,
    pending_events: Vec<Event<A::SupersetEvent>>,
}

impl<A, Tx> AggregateTransaction<A, Tx>
where
    A: TransactionalAggregate<Transaction = Tx>,
    Tx: Send,
{
    /// Handles a command within this transaction.
    ///
    /// Events and state changes are written to the transaction but not yet committed.
    /// Call `commit()` to persist all changes atomically.
    pub async fn handle(
        &mut self,
        command: Command<A::CommandData, A::CommandCredentials>,
    ) -> Result<Option<<A as EventApplicator<A::SupersetEvent>>::State>, HandleInTxError<A>> {
        // ... same logic as Aggregate::handle, but using self.transaction ...
        // ... events added to self.pending_events instead of publishing ...
    }

    /// Commits the transaction and publishes all buffered events.
    ///
    /// After this call, all events and state changes are durable.
    /// Events are published to the event bus in order.
    pub async fn commit(self) -> Result<(), CommitError<A::TransactionError, A::TransactionError>> {
        self.transaction.commit().await?;
        
        // Publish events after commit
        for event in self.pending_events {
            self.aggregate.publish_event(event).await?;
        }
        
        Ok(())
    }

    /// Rolls back the transaction, discarding all changes.
    ///
    /// No events are published. State changes are discarded.
    pub async fn rollback(self) -> Result<(), A::TransactionError> {
        self.transaction.rollback().await
    }
}
```

### 3.3 TransactionalAggregate Trait

```rust
/// An aggregate that supports transactional command handling.
///
/// Extends `Aggregate` with the ability to begin a transaction that wraps
/// multiple operations atomically.
///
/// # Type Parameter Relationship
///
/// The `SupersetEvent` associated type must match the `ED` parameter from the
/// `Aggregate<ED>` trait bound. This is enforced by the trait bound
/// `Aggregate<Self::SupersetEvent>`.
#[async_trait]
pub trait TransactionalAggregate: Aggregate<Self::SupersetEvent> + Sized
where
    <Self as EventApplicator<Self::SupersetEvent>>::State: AggregateState,
{
    /// The superset event type.
    ///
    /// Must match the `ED` parameter from the `Aggregate<ED>` implementation.
    /// This is the enum containing all event variants for the application.
    type SupersetEvent: EventData + Send + Sync + 'static;
    
    /// The transaction type (e.g., `sqlx::Transaction<'static, Postgres>`).
    type Transaction: Send;

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
    async fn begin(self: Arc<Self>) -> Result<
        AggregateTransaction<Self, Self::Transaction>,
        Self::TransactionError
    >;

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
```

### 3.4 Concurrency Control Strategy

For concurrent transactions on the same aggregate, we use **pessimistic locking** via PostgreSQL's `SELECT ... FOR UPDATE`:

```sql
-- get_state_in_tx acquires a row-level lock
SELECT * FROM my_state_table WHERE id = $1 FOR UPDATE;
```

This ensures:
1. Only one transaction can modify a given aggregate at a time
2. Concurrent transactions block until the first commits/rolls back
3. No lost updates or version mismatch surprises

**Why pessimistic over optimistic?**

- Optimistic concurrency (version checks) still works but results in retry overhead
- For transactions that may involve multiple `handle()` calls, pessimistic locking is more efficient
- The lock is held only for the transaction duration, which should be short

**Lock ordering:**

When a transaction handles multiple aggregates, acquire locks in a consistent order (e.g., by UUID) to prevent deadlocks.

> **Note:** Lock ordering is the caller's responsibility. The framework does not enforce this automatically. For multi-aggregate transactions (§10.3), a future `TransactionContext` helper could sort operations by aggregate ID before execution.

**PgState trait extension:**

The `PgState` trait needs a new method for locking reads:

```rust
#[async_trait]
pub trait PgState: Send + Sync + Sized + Unpin {
    /// Retrieve the entity with a row-level lock for update.
    ///
    /// Uses `SELECT ... FOR UPDATE` to prevent concurrent modifications.
    /// Returns `None` if the row doesn't exist (no lock acquired).
    async fn find_by_id_for_update<'a, T>(id: Uuid, executor: T) -> Result<Option<Self>, sqlx::Error>
    where
        T: PgExecutor<'a>;

    // ... existing methods unchanged ...
}
```

### 3.5 PostgreSQL Implementation

```rust
/// PostgreSQL-specific aggregate with transaction support.
///
/// Requires that both event store and state store use the same PgPool.
pub struct PgAggregate<S, E, B>
where
    S: PgState + AggregateState,
    E: EventData,
    B: EventBus<EventType = E>,
{
    pool: PgPool,
    event_store: PgEventStore<B>,
    _phantom: PhantomData<S>,
}

/// Errors for PgAggregate operations.
#[derive(Debug, thiserror::Error)]
pub enum PgAggregateError<BE>
where
    BE: std::error::Error,
{
    /// Database error
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    
    /// Event store error
    #[error("Event store error: {0}")]
    EventStore(PgEventStoreError<BE>),
}

impl<BE> From<PgEventStoreError<BE>> for PgAggregateError<BE>
where
    BE: std::error::Error,
{
    fn from(e: PgEventStoreError<BE>) -> Self {
        PgAggregateError::EventStore(e)
    }
}

#[async_trait]
impl<S, E, B> TransactionalAggregate for PgAggregate<S, E, B>
where
    S: PgState + AggregateState + Send + Sync,
    E: EventData + Send + Sync + 'static,
    B: EventBus<EventType = E> + Send + Sync + Clone + 'static,
    B::Error: Send + Sync + 'static,
{
    type SupersetEvent = E;
    type Transaction = sqlx::Transaction<'static, Postgres>;
    type TransactionError = PgAggregateError<B::Error>;

    async fn begin(self: Arc<Self>) -> Result<AggregateTransaction<Self, Self::Transaction>, Self::TransactionError> {
        let tx = self.pool.begin().await?;
        Ok(AggregateTransaction {
            aggregate: self,
            transaction: tx,
            pending_events: Vec::new(),
        })
    }

    async fn store_events_in_tx(
        &self,
        tx: &mut Self::Transaction,
        events: Vec<Event<E>>,
    ) -> Result<Vec<Event<E>>, Self::TransactionError> {
        self.event_store.store_events_in_tx(tx, events).await
            .map_err(Into::into)
    }

    async fn get_state_in_tx(
        &self,
        tx: &mut Self::Transaction,
        id: Uuid,
    ) -> Result<Option<S>, Self::TransactionError> {
        // Uses FOR UPDATE to acquire row-level lock
        S::find_by_id_for_update(id, &mut **tx).await
            .map_err(Into::into)
    }

    async fn persist_state_in_tx(
        &self,
        tx: &mut Self::Transaction,
        id: Uuid,
        state: S,
    ) -> Result<(), Self::TransactionError> {
        S::upsert(id, &state, &mut **tx).await
            .map_err(Into::into)
    }

    async fn delete_state_in_tx(
        &self,
        tx: &mut Self::Transaction,
        id: Uuid,
    ) -> Result<(), Self::TransactionError> {
        S::delete(id, &mut **tx).await
            .map_err(Into::into)
    }

    async fn publish_event(&self, event: Event<E>) -> Result<(), Self::TransactionError> {
        self.event_store.publish_events(vec![event]).await
            .map_err(Into::into)
    }
}
```

### 3.6 InMemory Implementation

The in-memory implementation simulates transactions for testing. It uses a write-ahead log pattern:
changes are buffered and only applied on commit.

```rust
/// In-memory aggregate with simulated transaction support.
pub struct InMemoryAggregate<S, E, B>
where
    S: AggregateState + Clone,
    E: EventData,
    B: EventBus<EventType = E>,
{
    event_store: InMemoryEventStore<B>,
    state_store: InMemoryStateStore<S>,
}

/// Pending changes for an in-memory transaction.
///
/// Buffers all changes until commit, then applies them atomically.
pub struct InMemoryTransaction<S, E, B>
where
    S: Clone,
    E: EventData,
    B: EventBus<EventType = E>,
{
    /// Snapshot of state at transaction start (for rollback)
    state_snapshot: HashMap<Uuid, Option<S>>,
    /// Pending state changes (id -> new state or None for delete)
    pending_states: HashMap<Uuid, Option<S>>,
    /// Pending events to store
    pending_events: Vec<Event<E>>,
    /// Reference to the actual stores (for commit)
    event_store: InMemoryEventStore<B>,
    state_store: InMemoryStateStore<S>,
    /// Lock guard held for transaction duration
    _lock: tokio::sync::OwnedMutexGuard<()>,
}

impl<S, E, B> InMemoryTransaction<S, E, B>
where
    S: Clone + Send + Sync,
    E: EventData + Send + Sync,
    B: EventBus<EventType = E>,
{
    /// Commits all pending changes atomically.
    pub async fn commit(self) -> Result<(), InMemoryTransactionError> {
        // Apply all pending state changes
        for (id, state) in self.pending_states {
            match state {
                Some(s) => self.state_store.persist_state(id, s).await?,
                None => self.state_store.delete_state(id).await?,
            }
        }
        
        // Store all pending events
        self.event_store.store_events(self.pending_events).await?;
        
        // Lock is released when self is dropped
        Ok(())
    }
    
    /// Rolls back the transaction, discarding all changes.
    pub async fn rollback(self) -> Result<(), InMemoryTransactionError> {
        // Simply drop self - no changes were applied
        // Lock is released when self is dropped
        Ok(())
    }
}

/// Errors for in-memory aggregate transactions.
#[derive(Debug, thiserror::Error)]
pub enum InMemoryAggregateError<BE>
where
    BE: std::error::Error,
{
    /// Event store error
    #[error("Event store error: {0}")]
    EventStore(#[from] InMemoryEventStoreBackendError),
    
    /// State store error  
    #[error("State store error: {0}")]
    StateStore(#[from] InMemoryStateStoreError),
    
    /// Event publishing error
    #[error("Event publish error: {0}")]
    Publish(BE),
}

#[async_trait]
impl<S, E, B> TransactionalAggregate for InMemoryAggregate<S, E, B>
where
    S: AggregateState + Clone + Send + Sync + 'static,
    E: EventData + Send + Sync + 'static,
    B: EventBus<EventType = E> + Send + Sync + Clone + 'static,
    B::Error: Send + Sync + 'static,
{
    type SupersetEvent = E;
    type Transaction = InMemoryTransaction<S, E, B>;
    type TransactionError = InMemoryAggregateError<B::Error>;

    async fn begin(self: Arc<Self>) -> Result<AggregateTransaction<Self, Self::Transaction>, Self::TransactionError> {
        // Acquire exclusive lock for the transaction duration
        // This simulates PostgreSQL's transaction isolation
        let lock = self.transaction_lock.clone().lock_owned().await;
        
        let tx = InMemoryTransaction {
            state_snapshot: HashMap::new(),
            pending_states: HashMap::new(),
            pending_events: Vec::new(),
            event_store: self.event_store.clone(),
            state_store: self.state_store.clone(),
            _lock: lock,
        };
        
        Ok(AggregateTransaction {
            aggregate: self,
            transaction: tx,
            pending_events: Vec::new(),
        })
    }

    async fn get_state_in_tx(
        &self,
        tx: &mut Self::Transaction,
        id: Uuid,
    ) -> Result<Option<S>, Self::TransactionError> {
        // Check pending changes first (read-your-writes)
        if let Some(pending) = tx.pending_states.get(&id) {
            return Ok(pending.clone());
        }
        
        // Read from actual store and snapshot for potential rollback
        let state = self.state_store.get_state(id).await?;
        tx.state_snapshot.entry(id).or_insert_with(|| state.clone());
        Ok(state)
    }

    async fn persist_state_in_tx(
        &self,
        tx: &mut Self::Transaction,
        id: Uuid,
        state: S,
    ) -> Result<(), Self::TransactionError> {
        tx.pending_states.insert(id, Some(state));
        Ok(())
    }

    async fn delete_state_in_tx(
        &self,
        tx: &mut Self::Transaction,
        id: Uuid,
    ) -> Result<(), Self::TransactionError> {
        tx.pending_states.insert(id, None);
        Ok(())
    }

    async fn store_events_in_tx(
        &self,
        tx: &mut Self::Transaction,
        events: Vec<Event<E>>,
    ) -> Result<Vec<Event<E>>, Self::TransactionError> {
        // Buffer events for commit
        tx.pending_events.extend(events.clone());
        Ok(events)
    }

    async fn publish_event(&self, event: Event<E>) -> Result<(), Self::TransactionError> {
        self.event_store.bus().publish(Arc::new(event)).await
            .map_err(InMemoryAggregateError::Publish)
    }
}
```

**Key design decisions for InMemory:**

1. **Mutex for isolation**: A single mutex ensures only one transaction runs at a time, simulating PostgreSQL's serializable isolation
2. **Buffered writes**: All changes go to `pending_*` maps, applied only on commit
3. **Read-your-writes**: `get_state_in_tx` checks pending changes first
4. **Automatic rollback on drop**: If `commit()` isn't called, changes are discarded

## 4. AggregateTransaction::handle Implementation

The `handle()` method on `AggregateTransaction` mirrors `Aggregate::handle()` but uses transactional methods.

### 4.1 Version Tracking Across Multiple Handles

When multiple `handle()` calls occur within the same transaction on the **same aggregate**, the version must be tracked correctly:

```
Transaction:
  handle(cmd1) -> events [v1, v2] -> state.version = 2
  handle(cmd2) -> events [v3, v4] -> state.version = 4  // continues from 2
  commit()
```

The `AggregateTransaction` tracks the **last known state per aggregate ID** to ensure subsequent handles see the updated version:

```rust
pub struct AggregateTransaction<A, Tx>
where
    A: TransactionalAggregate,
{
    aggregate: Arc<A>,
    transaction: Tx,
    pending_events: Vec<Event<...>>,
    /// Cached state from previous handle() calls in this transaction.
    /// Ensures read-your-writes semantics for version tracking.
    state_cache: HashMap<Uuid, Option<<A as EventApplicator<...>>::State>>,
}
```

### 4.2 Implementation

```rust
impl<A, Tx> AggregateTransaction<A, Tx>
where
    A: TransactionalAggregate<Transaction = Tx>,
    Tx: Send,
{
    pub async fn handle(
        &mut self,
        command: Command<A::CommandData, A::CommandCredentials>,
    ) -> Result<Option<<A as EventApplicator<A::SupersetEvent>>::State>, HandleInTxError<A>> {
        if let Ok(cmd) = command.to_subset_command() {
            let state_id = self.aggregate.get_id_from_command(&command);
            
            // First check our cache (for subsequent handles in same tx)
            // Then fall back to reading from transaction
            let state = if let Some(cached) = self.state_cache.get(&state_id) {
                cached.clone()
            } else {
                self.aggregate
                    .get_state_in_tx(&mut self.transaction, state_id)
                    .await
                    .map_err(HandleCommandError::State)?
            };

            // Version check against current state (cached or fresh)
            let state_version = state.as_ref().map(|s| s.get_version()).unwrap_or(0);
            if let Some(expected) = command.aggregate_version {
                if state_version != expected {
                    return Err(HandleCommandError::VersionMismatch {
                        expected,
                        found: state_version,
                    });
                }
            }

            // Re-hydrate if needed (only on first access, cache handles subsequent)
            // For cached state, re-hydration already happened in previous handle()
            let state = if self.state_cache.contains_key(&state_id) {
                state  // Already up-to-date from previous handle()
            } else if let Some(state) = state {
                // First access: re-hydrate from event store
                let event_store = self.aggregate.get_event_store();
                let stream = event_store
                    .read_events_since(state_id, state.get_version() + 1)
                    .await
                    .map_err(HandleCommandError::Event)?;
                self.aggregate
                    .re_hydrate(Some(state), stream)
                    .await
                    .map_err(HandleCommandError::Hydration)?
            } else {
                None
            };

            // Calculate next version based on (possibly re-hydrated) state
            let base_version = state.as_ref().map(|s| s.get_version()).unwrap_or(0);
            let mut next_version = base_version + 1;

            // Handle command to produce events
            let events: Vec<Event<A::SupersetEvent>> = self.aggregate
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

            // Track the final version after all events
            let final_version = if events.is_empty() {
                base_version
            } else {
                events.last().unwrap().stream_version
            };

            // Apply events to state
            let event_stream = Box::pin(SliceRefEventStream::from(events.as_slice()));
            let state = self.aggregate
                .re_hydrate_from_refs(state, event_stream)
                .await
                .map_err(HandleCommandError::Hydration)?
                .map(|mut s| {
                    s.set_version(final_version);
                    s
                });

            // Store events in transaction (no publish yet)
            let stored_events = self.aggregate
                .store_events_in_tx(&mut self.transaction, events)
                .await
                .map_err(HandleCommandError::Event)?;

            // Buffer events for publishing after commit
            self.pending_events.extend(stored_events);

            // Store state in transaction
            if let Some(ref state) = state {
                self.aggregate
                    .persist_state_in_tx(&mut self.transaction, state_id, state.clone())
                    .await
                    .map_err(HandleCommandError::State)?;
            } else {
                self.aggregate
                    .delete_state_in_tx(&mut self.transaction, state_id)
                    .await
                    .map_err(HandleCommandError::State)?;
            }

            // Cache state for subsequent handle() calls on same aggregate
            self.state_cache.insert(state_id, state.clone());

            Ok(state)
        } else {
            Ok(None)
        }
    }
}
```

### 4.3 Version Continuity Example

```rust
let mut tx = aggregate.begin().await?;

// First handle: aggregate doesn't exist yet
tx.handle(CreateUser { id, name: "Alice" }).await?;
// -> UserCreated event at version 1
// -> state.version = 1, cached

// Second handle: uses cached state (version 1)
tx.handle(UpdateUser { id, name: "Bob" }).await?;
// -> UserUpdated event at version 2
// -> state.version = 2, cache updated

// Third handle: uses cached state (version 2)  
tx.handle(UpdateUser { id, name: "Carol" }).await?;
// -> UserUpdated event at version 3
// -> state.version = 3, cache updated

tx.commit().await?;
// Events [v1, v2, v3] stored atomically
// State with version=3 stored atomically
```

## 5. Relationship with Existing Aggregate Trait

The existing `Aggregate::handle()` method remains unchanged for backward compatibility. Users who don't need atomic events+state can continue using it.

### 5.1 Why Not Have `handle()` Internally Use `begin()`/`commit()`?

We considered having the existing `Aggregate::handle()` internally call `begin()` and `commit()` to get atomicity by default. This was rejected for several reasons:

1. **`Arc<Self>` requirement propagates**: `begin()` takes `self: Arc<Self>` to allow the transaction to outlive scope boundaries. If `handle()` called `begin()` internally, it would also need to take `Arc<Self>`, which is a **breaking API change**.

2. **Performance overhead for in-memory**: The in-memory implementation uses a mutex for transaction isolation. Non-transactional use cases would pay this cost unnecessarily.

3. **Semantic difference**: `handle()` is "fire and forget" while transactions have explicit lifecycle (`begin`/`commit`/`rollback`). Mixing these semantics would be confusing.

4. **Clone bound or allocation**: To create an `Arc<Self>` from `&self`, we'd need either `Self: Clone` (new trait bound) or to already have the aggregate in an `Arc`.

**Recommendation:** Users who want atomic-by-default behavior should wrap their aggregate in `Arc` and always use the transactional API. The non-transactional `handle()` remains available for simple use cases and backward compatibility.

For PostgreSQL users who need atomicity:

| Use Case | API |
|----------|-----|
| Simple, non-atomic | `aggregate.handle(cmd)` |
| Atomic single command | `aggregate.begin().handle(cmd).commit()` |
| Atomic batch (seeding) | `aggregate.begin(); for cmd { tx.handle(cmd) }; tx.commit()` |

## 6. Error Handling

### 6.1 CommitError

```rust
#[derive(Debug, thiserror::Error)]
pub enum CommitError<T, P> {
    #[error("Transaction commit failed: {0}")]
    Transaction(T),
    
    #[error("Event publishing failed after commit: {0}")]
    Publish(P),
}
```

Note: If publishing fails after commit, events are durable but not published. This is acceptable - projections can catch up via polling or the reliable delivery mechanism.

### 6.2 Automatic Rollback on Drop

If `AggregateTransaction` is dropped without calling `commit()`, the transaction is rolled back:

```rust
impl<A, Tx> Drop for AggregateTransaction<A, Tx>
where
    A: TransactionalAggregate,
{
    fn drop(&mut self) {
        // Transaction will be rolled back when dropped if not committed
        // (This is sqlx's default behavior for Transaction<'_, Postgres>)
        // For InMemoryTransaction, pending changes are simply discarded
    }
}
```

## 7. Files to Modify

### 7.1 epoch_core

| File | Changes |
|------|---------|
| `src/aggregate.rs` | Add `TransactionalAggregate` trait |
| `src/aggregate.rs` | Add `AggregateTransaction` struct with `state_cache` |
| `src/aggregate.rs` | Add `CommitError` enum |
| `src/lib.rs` | Export new types |

### 7.2 epoch_pg

| File | Changes |
|------|---------|
| `src/aggregate.rs` | New file: `PgAggregate` implementation |
| `src/aggregate.rs` | Add `PgAggregateError` enum |
| `src/state_store.rs` | Add `find_by_id_for_update` to `PgState` trait |
| `src/event_store.rs` | Ensure `store_events_in_tx` is public (already done) |
| `src/lib.rs` | Export `PgAggregate`, `PgAggregateError` |

### 7.3 epoch_mem

| File | Changes |
|------|---------|
| `src/aggregate.rs` | New file: `InMemoryAggregate` implementation |
| `src/aggregate.rs` | Add `InMemoryTransaction` struct |
| `src/aggregate.rs` | Add `InMemoryAggregateError` enum |
| `src/event_store.rs` | Add `transaction_lock: Arc<Mutex<()>>` to `InMemoryAggregate` |
| `src/lib.rs` | Export types |

## 8. Testing Strategy

### 8.1 Unit Tests

1. **`transaction_handle_same_as_aggregate_handle`**: Verify same result
2. **`transaction_rollback_discards_changes`**: Verify no persistence on rollback
3. **`transaction_commit_persists_events_and_state`**: Verify both are durable
4. **`transaction_publishes_after_commit`**: Verify event bus receives events after commit
5. **`transaction_batch_multiple_commands`**: Verify multiple handles in one transaction

### 8.2 Integration Tests (PostgreSQL)

1. **`test_atomic_events_and_state`**:
   - Begin transaction
   - Handle command
   - Verify events and state not visible outside transaction
   - Commit
   - Verify both visible

2. **`test_rollback_on_error`**:
   - Begin transaction
   - Handle first command (succeeds)
   - Handle second command (fails)
   - Verify first command's changes are rolled back

3. **`test_batch_seeding_performance`**:
   - Seed 1000 entities via individual `handle()` calls
   - Seed 1000 entities via batched transaction
   - Verify batched is significantly faster

4. **`test_concurrent_transactions_block`**:
   - Start transaction A on aggregate X
   - Start transaction B on same aggregate X (should block on `get_state_in_tx`)
   - Commit A
   - B unblocks, sees A's changes
   - B can proceed with updated state

5. **`test_version_continuity_in_transaction`**:
   - Begin transaction
   - Handle 3 commands on same aggregate
   - Verify events have versions [1, 2, 3]
   - Verify final state.version = 3
   - Commit and verify persisted correctly

## 9. Migration Guide

### 9.1 For Existing Users

No changes required. `Aggregate::handle()` continues to work as before.

### 9.2 For Atomic Operations

Replace:
```rust
aggregate.handle(command).await?;
```

With:
```rust
let aggregate = Arc::new(aggregate);
let mut tx = aggregate.clone().begin().await?;
tx.handle(command).await?;
tx.commit().await?;
```

### 9.3 For Batch Operations (Seeding)

Replace:
```rust
for command in commands {
    aggregate.handle(command).await?;  // N fsyncs
}
```

With:
```rust
let aggregate = Arc::new(aggregate);
let mut tx = aggregate.clone().begin().await?;
for command in commands {
    tx.handle(command).await?;
}
tx.commit().await?;  // 1 fsync
```

## 10. Future Considerations

### 10.1 Macro Support

A proc-macro could auto-implement `TransactionalAggregate` for PostgreSQL aggregates:

```rust
#[derive(TransactionalAggregate)]
#[aggregate(pool = "self.pool", event_store = "self.event_store")]
struct MyAggregate { ... }
```

### 10.2 Nested Transactions / Savepoints

For complex workflows, savepoints could be added:

```rust
let mut tx = aggregate.begin().await?;
tx.handle(cmd1).await?;
let savepoint = tx.savepoint().await?;
if tx.handle(cmd2).await.is_err() {
    tx.rollback_to(savepoint).await?;
}
tx.commit().await?;
```

### 10.3 Cross-Aggregate Transactions

For saga patterns where multiple aggregates need atomic updates:

```rust
let mut tx = TransactionContext::begin(&pool).await?;
tx.handle(&user_aggregate, user_cmd).await?;
tx.handle(&order_aggregate, order_cmd).await?;
tx.commit().await?;
```

## 11. Summary

| Component | Description |
|-----------|-------------|
| `TransactionalAggregate` trait | Defines transaction lifecycle methods (`begin`, `*_in_tx`) |
| `AggregateTransaction` struct | Wrapper with `handle()` + `commit()`/`rollback()`, includes `state_cache` for version tracking |
| `CommitError` enum | Distinguishes transaction vs publish failures |
| `PgAggregate` | PostgreSQL implementation with `FOR UPDATE` locking |
| `PgAggregateError` | Error type for PostgreSQL transactions |
| `InMemoryAggregate` | In-memory implementation for testing |
| `InMemoryTransaction` | Buffered changes with mutex-based isolation |

This design provides:
- **True atomicity**: Events and state in same transaction
- **Familiar API**: Same `handle()` method
- **Batch support**: Multiple commands in one transaction with correct version continuity
- **Backward compatible**: Existing code unchanged
- **Publish after commit**: Events only go to bus after durability guaranteed
- **Pessimistic locking**: `SELECT ... FOR UPDATE` prevents lost updates
- **Read-your-writes**: State cache ensures version continuity across handles
