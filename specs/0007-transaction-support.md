# Specification: Transaction Support

**Status:** Draft  
**Created:** 2026-01-26  
**Author:** AI Agent  

## 1. Problem Statement

The current `Aggregate::handle` implementation performs multiple individual database writes without transaction support:

```rust
async fn handle(&self, command: Command<...>) -> Result<...> {
    // ... handle command, produce events ...
    
    // Each event is a separate write (separate fsync)
    for event in events.into_iter() {
        self.get_event_store().store_event(event).await?;
    }
    
    // State is another separate write (another fsync)
    state_store.persist_state(state.get_id(), state).await?;
}
```

This causes several problems:

### 1.1 Performance: fsync Per Write

Without transactions, PostgreSQL performs `fsync()` after each write to ensure durability. For a seed script with ~10k writes, this means ~10k fsync operations.

**Measured impact:** A seed script generating ~2,500 entities and ~8,000 events takes **several minutes** instead of seconds.

### 1.2 Atomicity: Partial Writes on Failure

If the process crashes mid-command:
- Some events may be persisted, others not
- Events may be persisted but state not updated
- Leaves the system in an inconsistent state

### 1.3 Consistency: Events and State Can Diverge

Even without crashes, the window between event writes and state write allows:
- Another instance to read stale state
- Concurrent commands to see partial event sequences

## 2. Proposed Solution

Add transaction support at two levels:

1. **Command-Level Transactions**: Each `handle()` call is atomic (events + state)
2. **Batch Transactions**: Multiple commands can be grouped in a single transaction

### 2.1 Expected Performance Impact

| Scenario | Current | With Transactions |
|----------|---------|-------------------|
| Single command (2 events + state) | 3 fsyncs | 1 fsync |
| Seed script (~10k writes) | ~10k fsyncs | 1 fsync (batch) or ~7k fsyncs (command-level) |
| **Seed time** | **Several minutes** | **Seconds (batch) or ~1 min (command-level)** |

### 2.2 Additional Benefits

- **Atomicity**: All-or-nothing writes per command
- **Consistency**: No partial event sequences visible
- **Rollback on error**: Failed commands don't leave partial state
- **Foundation for sagas**: Required for cross-aggregate coordination

## 3. API Design

### 3.1 Transaction Trait

```rust
/// A database transaction that can be committed or rolled back.
#[async_trait]
pub trait Transaction: Send + Sync {
    /// The error type for transaction operations.
    type Error: std::error::Error + Send + Sync + 'static;
    
    /// Commits all operations in this transaction.
    async fn commit(self) -> Result<(), Self::Error>;
    
    /// Rolls back all operations in this transaction.
    async fn rollback(self) -> Result<(), Self::Error>;
}
```

### 3.2 Extended EventStoreBackend

```rust
#[async_trait]
pub trait EventStoreBackend: Send + Sync {
    type EventType: EventData;
    type Error: std::error::Error + Send + Sync + 'static;
    type Transaction: Transaction<Error = Self::Error>;
    
    // Existing methods (unchanged)
    async fn store_event(&self, event: Event<Self::EventType>) -> Result<(), Self::Error>;
    async fn read_events(...) -> Result<...>;
    
    // New: transaction support
    /// Begins a new transaction.
    async fn begin_transaction(&self) -> Result<Self::Transaction, Self::Error>;
    
    /// Stores an event within a transaction.
    async fn store_event_in_tx(
        &self,
        tx: &mut Self::Transaction,
        event: Event<Self::EventType>,
    ) -> Result<(), Self::Error>;
    
    /// Stores multiple events within a transaction.
    async fn store_events_in_tx(
        &self,
        tx: &mut Self::Transaction,
        events: Vec<Event<Self::EventType>>,
    ) -> Result<(), Self::Error> {
        for event in events {
            self.store_event_in_tx(tx, event).await?;
        }
        Ok(())
    }
}
```

### 3.3 Extended StateStoreBackend

```rust
#[async_trait]
pub trait StateStoreBackend<S>: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;
    type Transaction: Transaction<Error = Self::Error>;
    
    // Existing methods (unchanged)
    async fn get_state(&self, id: Uuid) -> Result<Option<S>, Self::Error>;
    async fn persist_state(&mut self, id: Uuid, state: S) -> Result<(), Self::Error>;
    async fn delete_state(&mut self, id: Uuid) -> Result<(), Self::Error>;
    
    // New: transaction support
    /// Begins a new transaction.
    async fn begin_transaction(&self) -> Result<Self::Transaction, Self::Error>;
    
    /// Persists state within a transaction.
    async fn persist_state_in_tx(
        &self,
        tx: &mut Self::Transaction,
        id: Uuid,
        state: S,
    ) -> Result<(), Self::Error>;
    
    /// Deletes state within a transaction.
    async fn delete_state_in_tx(
        &self,
        tx: &mut Self::Transaction,
        id: Uuid,
    ) -> Result<(), Self::Error>;
}
```

### 3.4 Unified Transaction (PostgreSQL)

For PostgreSQL, both event store and state store use the same connection pool, so they can share a transaction:

```rust
/// A PostgreSQL transaction wrapping sqlx::Transaction.
pub struct PgTransaction<'a> {
    inner: sqlx::Transaction<'a, sqlx::Postgres>,
}

impl Transaction for PgTransaction<'_> {
    type Error = sqlx::Error;
    
    async fn commit(self) -> Result<(), Self::Error> {
        self.inner.commit().await
    }
    
    async fn rollback(self) -> Result<(), Self::Error> {
        self.inner.rollback().await
    }
}
```

### 3.5 Modified Aggregate::handle

```rust
#[async_trait]
pub trait Aggregate<ED>: Projection<ED> {
    // ... existing associated types ...
    
    /// Handles a command within an implicit transaction.
    /// 
    /// All event writes and the state write are atomic.
    async fn handle(
        &self,
        command: Command<Self::CommandData, Self::CommandCredentials>,
    ) -> Result<Option<Self::State>, HandleCommandError<...>> {
        // Begin transaction
        let mut tx = self.get_event_store().begin_transaction().await
            .map_err(HandleCommandError::Event)?;
        
        // ... existing logic: get state, re-hydrate, handle command ...
        
        // Store events in transaction
        for event in events.into_iter() {
            self.get_event_store()
                .store_event_in_tx(&mut tx, event)
                .await
                .map_err(HandleCommandError::Event)?;
        }
        
        // Persist state in transaction
        if let Some(ref state) = state {
            self.get_state_store()
                .persist_state_in_tx(&mut tx, state.get_id(), state.clone())
                .await
                .map_err(HandleCommandError::State)?;
        } else {
            self.get_state_store()
                .delete_state_in_tx(&mut tx, state_id)
                .await
                .map_err(HandleCommandError::State)?;
        }
        
        // Commit transaction
        tx.commit().await.map_err(HandleCommandError::Event)?;
        
        Ok(state)
    }
}
```

### 3.6 Batch Transaction API

For bulk operations (like seeding), provide an explicit transaction API:

```rust
/// A batch transaction for executing multiple commands atomically.
pub struct BatchTransaction<'a, A, ED>
where
    A: Aggregate<ED>,
    ED: EventData,
{
    aggregate: &'a A,
    tx: <A::EventStore as EventStoreBackend>::Transaction,
    // Track pending state writes
    pending_states: HashMap<Uuid, Option<A::State>>,
}

impl<'a, A, ED> BatchTransaction<'a, A, ED>
where
    A: Aggregate<ED>,
    ED: EventData + Send + Sync + 'static,
{
    /// Handles a command within this batch transaction.
    /// 
    /// Events and state changes are buffered until `commit()` is called.
    pub async fn handle(
        &mut self,
        command: Command<A::CommandData, A::CommandCredentials>,
    ) -> Result<Option<A::State>, HandleCommandError<...>> {
        // Get state: check pending_states first, then store
        let state_id = self.aggregate.get_id_from_command(&command);
        let state = match self.pending_states.get(&state_id) {
            Some(s) => s.clone(),
            None => self.aggregate.get_state_store().get_state(state_id).await?,
        };
        
        // Re-hydrate, handle command, etc.
        // ...
        
        // Store events in transaction
        self.aggregate.get_event_store()
            .store_events_in_tx(&mut self.tx, events)
            .await?;
        
        // Buffer state (don't write yet)
        self.pending_states.insert(state_id, state.clone());
        
        Ok(state)
    }
    
    /// Commits all commands in this batch.
    pub async fn commit(mut self) -> Result<(), ...> {
        // Write all pending states
        for (id, state) in self.pending_states {
            match state {
                Some(s) => {
                    self.aggregate.get_state_store()
                        .persist_state_in_tx(&mut self.tx, id, s)
                        .await?;
                }
                None => {
                    self.aggregate.get_state_store()
                        .delete_state_in_tx(&mut self.tx, id)
                        .await?;
                }
            }
        }
        
        // Commit transaction
        self.tx.commit().await
    }
    
    /// Rolls back all commands in this batch.
    pub async fn rollback(self) -> Result<(), ...> {
        self.tx.rollback().await
    }
}
```

**Usage:**

```rust
// Seed script with batch transaction
let mut tx = aggregate.begin_batch_transaction().await?;

for entity_data in seed_data {
    tx.handle(CreateCommand { data: entity_data }).await?;
}

tx.commit().await?;  // Single fsync for entire seed
```

## 4. Implementation Details

### 4.1 Shared Transaction Between Event Store and State Store

For PostgreSQL, both stores share the same connection pool. The challenge is that `sqlx::Transaction` requires ownership or exclusive mutable reference.

**Option A: Transaction wrapper passed to both stores**

```rust
// Both stores accept the same transaction reference
event_store.store_event_in_tx(&mut tx, event).await?;
state_store.persist_state_in_tx(&mut tx, id, state).await?;
```

**Option B: Unified store that handles both**

```rust
pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    async fn begin_transaction(&self) -> Result<PgTransaction, Error>;
    async fn store_event_in_tx(&self, tx: &mut PgTransaction, event: Event<ED>);
    async fn persist_state_in_tx<S>(&self, tx: &mut PgTransaction, id: Uuid, state: S);
}
```

**Recommendation:** Option A is more flexible and maintains separation of concerns.

### 4.2 Transaction Lifetime and Borrowing

`sqlx::Transaction` has a lifetime tied to the pool. This can be tricky with async and the borrow checker.

```rust
pub struct PgTransaction<'a> {
    inner: sqlx::Transaction<'a, sqlx::Postgres>,
}

// The transaction must live at least as long as operations using it
async fn handle_with_tx<'a>(
    event_store: &PgEventStore,
    tx: &'a mut PgTransaction<'a>,
    event: Event<ED>,
) -> Result<(), Error> {
    // ...
}
```

### 4.3 Read Operations Within Transactions

For re-hydration within a transaction, reads should see uncommitted writes:

```rust
async fn read_events_in_tx(
    &self,
    tx: &mut Self::Transaction,
    stream_id: Uuid,
    since_version: u64,
) -> Result<Vec<Event<Self::EventType>>, Self::Error>;
```

This ensures that when handling multiple commands in a batch, later commands see events from earlier commands.

### 4.4 Error Handling and Rollback

Transactions should automatically rollback on drop if not committed:

```rust
impl Drop for PgTransaction<'_> {
    fn drop(&mut self) {
        // sqlx::Transaction already handles this - rollback on drop
    }
}
```

Explicit error handling:

```rust
let mut tx = event_store.begin_transaction().await?;

match do_work(&mut tx).await {
    Ok(result) => {
        tx.commit().await?;
        Ok(result)
    }
    Err(e) => {
        tx.rollback().await?;  // Explicit rollback
        Err(e)
    }
}
```

### 4.5 In-Memory Implementation

For `epoch_mem`, transactions can be simulated with buffered writes:

```rust
pub struct MemTransaction<ED> {
    pending_events: Vec<Event<ED>>,
    pending_states: HashMap<Uuid, Option<Box<dyn Any + Send + Sync>>>,
}

impl<ED> Transaction for MemTransaction<ED> {
    async fn commit(self) -> Result<(), Self::Error> {
        // Apply all pending writes to the in-memory store
    }
    
    async fn rollback(self) -> Result<(), Self::Error> {
        // Just drop pending writes
        Ok(())
    }
}
```

## 5. Files to Modify

### 5.1 epoch_core

1. **`src/transaction.rs`** (new):
   - `Transaction` trait
   - `BatchTransaction` struct

2. **`src/event_store.rs`**:
   - Add `Transaction` associated type to `EventStoreBackend`
   - Add `begin_transaction()` method
   - Add `store_event_in_tx()` method
   - Add `store_events_in_tx()` method

3. **`src/state_store.rs`**:
   - Add `Transaction` associated type to `StateStoreBackend`
   - Add `begin_transaction()` method
   - Add `persist_state_in_tx()` method
   - Add `delete_state_in_tx()` method

4. **`src/aggregate.rs`**:
   - Modify `handle()` to use transactions
   - Add `begin_batch_transaction()` method

5. **`src/lib.rs`**:
   - Re-export transaction types

### 5.2 epoch_pg

1. **`src/transaction.rs`** (new):
   - `PgTransaction` struct wrapping `sqlx::Transaction`

2. **`src/event_store.rs`**:
   - Implement transaction methods for `PgEventStore`

3. **`src/state_store.rs`** (if exists, or within aggregate implementations):
   - Implement transaction methods for PostgreSQL state store

### 5.3 epoch_mem

1. **`src/transaction.rs`** (new):
   - `MemTransaction` struct with buffered writes

2. **`src/event_store.rs`**:
   - Implement transaction methods for `MemEventStore`

3. **`src/state_store.rs`**:
   - Implement transaction methods for `MemStateStore`

## 6. Migration Strategy

### 6.1 Backward Compatibility

The change to use transactions in `handle()` is **not backward compatible** at the trait level, but is **behaviorally compatible**:

- Existing code calling `handle()` will work the same (but faster and atomic)
- Custom `EventStoreBackend` implementations will need to add transaction methods

### 6.2 Phased Rollout

**Phase 1: Add transaction infrastructure (non-breaking)**
- Add `Transaction` trait
- Add transaction methods to backends (with default no-op implementations)
- Implement for `PgEventStore`, `PgStateStore`, `MemEventStore`, `MemStateStore`

**Phase 2: Use transactions in handle() (breaking for custom backends)**
- Modify `Aggregate::handle()` to use transactions
- Update all backend implementations

**Phase 3: Add batch transaction API**
- Add `BatchTransaction`
- Add `begin_batch_transaction()` to `Aggregate`

## 7. Testing Strategy

### 7.1 Unit Tests

1. **`transaction_commit_persists_data`**: Verify committed transactions are durable
2. **`transaction_rollback_discards_data`**: Verify rolled back transactions leave no trace
3. **`transaction_auto_rollback_on_drop`**: Verify uncommitted transactions rollback
4. **`transaction_isolation`**: Verify concurrent transactions are isolated

### 7.2 Integration Tests

1. **`test_handle_is_atomic`**:
   - Start a command that would write 2 events + state
   - Inject failure after first event write
   - Verify no events or state were persisted

2. **`test_batch_transaction_commit`**:
   - Begin batch transaction
   - Handle 100 commands
   - Commit
   - Verify all events and states persisted

3. **`test_batch_transaction_rollback`**:
   - Begin batch transaction
   - Handle 100 commands
   - Rollback
   - Verify no events or states persisted

4. **`test_batch_transaction_sees_uncommitted_writes`**:
   - Begin batch transaction
   - Handle command A (creates entity)
   - Handle command B (references entity from A)
   - Verify command B can read entity A's state

5. **`test_concurrent_transactions_isolated`**:
   - Begin two transactions
   - Write different data in each
   - Verify each only sees its own writes until commit

### 7.3 Performance Tests

1. **`bench_seed_without_transactions`**: Current behavior baseline
2. **`bench_seed_with_command_transactions`**: Each command in its own transaction
3. **`bench_seed_with_batch_transaction`**: Entire seed in one transaction

Expected results:
- Command transactions: 2-3x faster than current
- Batch transaction: 10-100x faster than current

## 8. Backward Compatibility

### 8.1 Breaking Changes

- `EventStoreBackend` trait gains new required methods
- `StateStoreBackend` trait gains new required methods
- Custom implementations must be updated

### 8.2 Mitigation

Provide default implementations that fall back to non-transactional behavior:

```rust
#[async_trait]
pub trait EventStoreBackend {
    // ...
    
    /// Default: no-op transaction that commits immediately
    async fn begin_transaction(&self) -> Result<Self::Transaction, Self::Error> {
        Ok(NoOpTransaction)
    }
    
    /// Default: falls back to non-transactional store_event
    async fn store_event_in_tx(
        &self,
        _tx: &mut Self::Transaction,
        event: Event<Self::EventType>,
    ) -> Result<(), Self::Error> {
        self.store_event(event).await
    }
}
```

This allows gradual migration - existing backends work but don't get transactional benefits until updated.

## 9. Future Considerations

### 9.1 Distributed Transactions

For systems with event store and state store on different databases, distributed transactions (2PC) or saga patterns may be needed. This spec focuses on single-database transactions.

### 9.2 Transaction Hooks

Add hooks for custom logic within transactions:

```rust
trait TransactionHooks {
    async fn before_commit(&self, tx: &Transaction) -> Result<(), Error>;
    async fn after_commit(&self) -> Result<(), Error>;
    async fn after_rollback(&self) -> Result<(), Error>;
}
```

### 9.3 Nested Transactions / Savepoints

Support for savepoints within a transaction:

```rust
let mut tx = store.begin_transaction().await?;
let savepoint = tx.savepoint("before_risky_operation").await?;

match risky_operation(&mut tx).await {
    Ok(_) => { /* continue */ }
    Err(_) => {
        tx.rollback_to(savepoint).await?;
        // Try alternative approach
    }
}

tx.commit().await?;
```

### 9.4 Read-Only Transactions

For consistent reads across multiple queries:

```rust
let tx = store.begin_read_transaction().await?;
let events = store.read_events_in_tx(&tx, stream_id).await?;
let state = state_store.get_state_in_tx(&tx, id).await?;
// Both reads see consistent snapshot
```

## 10. Implementation Plan

### Phase 1: Core Infrastructure
1. Create `Transaction` trait in `epoch_core`
2. Add transaction associated type and methods to `EventStoreBackend`
3. Add transaction associated type and methods to `StateStoreBackend`
4. Provide default no-op implementations for backward compatibility
5. Add unit tests for transaction trait

### Phase 2: PostgreSQL Implementation
1. Create `PgTransaction` wrapper around `sqlx::Transaction`
2. Implement transaction methods in `PgEventStore`
3. Implement transaction methods in PostgreSQL state store
4. Add integration tests

### Phase 3: In-Memory Implementation
1. Create `MemTransaction` with buffered writes
2. Implement transaction methods in `MemEventStore`
3. Implement transaction methods in `MemStateStore`
4. Add unit tests

### Phase 4: Aggregate Integration
1. Modify `Aggregate::handle()` to use command-level transactions
2. Update all example aggregates
3. Add integration tests for atomic command handling

### Phase 5: Batch Transaction API
1. Implement `BatchTransaction` struct
2. Add `begin_batch_transaction()` to `Aggregate` trait
3. Add integration tests for batch operations
4. Add performance benchmarks

### Phase 6: Documentation & Migration Guide
1. Update rustdoc for all new APIs
2. Write migration guide for custom backend implementations
3. Update CHANGELOG.md
4. Add examples for seed script usage
