# Specification: StateWriteMode::Deferred

**Status:** Deferred (see Section 10)  
**Created:** 2026-01-26  
**Author:** AI Agent  

## 1. Problem Statement

In the current `Aggregate::handle` implementation, state is persisted to the state store after every command, regardless of how frequently commands are processed. While this ensures the state cache is always up-to-date, it introduces unnecessary overhead:

- **Database write per command**: Each successful command results in a state store write
- **High-throughput bottleneck**: For aggregates processing many commands per second, state writes become a limiting factor
- **Redundant work**: State can always be reconstructed from events via `re_hydrate`

Since events are the source of truth and are always persisted immediately, state persistence is purely an optimization to reduce re-hydration time. Deferring state writes can significantly improve throughput without sacrificing durability.

## 2. Proposed Solution

Add `StateWriteMode` enum to control when aggregate state is persisted:

1. **Immediate** (current behavior): Write state after every command
2. **Deferred**: Buffer state writes and persist when thresholds are crossed

### 2.1 Tradeoff Analysis

| Aspect | Immediate | Deferred |
|--------|-----------|----------|
| State writes | 1 per command | 1 per batch |
| Throughput | Lower | Higher |
| Re-hydration on startup | Minimal | More events to replay |
| Durability | Full (events always persisted) | Full (events always persisted) |
| Implementation complexity | Simple | Moderate |

### 2.2 Key Insight

**Events are the source of truth.** State is merely a cached projection of events. Since events are always persisted immediately, there is zero durability risk with deferred state writes. The only cost is increased re-hydration time after a crash or restart.

### 2.3 Deployment Model Requirements

`StateWriteMode::Deferred` maintains pending state **in instance-local memory**. This has implications for multi-instance deployments:

| Deployment Model | Deferred Mode | Notes |
|------------------|---------------|-------|
| Single instance | ✅ Works | Ideal use case |
| Green-blue deployment | ✅ Works | Brief overlap OK; flush on graceful shutdown |
| Sharded by aggregate ID | ✅ Works | Each aggregate handled by one instance |
| Leader election per aggregate | ✅ Works | Same as green-blue at failover |
| Horizontal scaling (same aggregate, multiple instances) | ⚠️ No benefit | Pending state not shared; use `Immediate` |

**Why horizontal scaling doesn't benefit:**
- Each instance maintains its own pending state
- Instance A's pending state is invisible to Instance B
- Both instances re-hydrate from stale store state anyway
- No correctness issues (re-hydration ensures correctness), but no performance benefit either

**Correctness guarantee:** Re-hydration from events always produces correct state, regardless of `StateWriteMode`. The mode only affects performance characteristics.

### 2.4 Use Cases

**Use Immediate when:**
- Low command volume
- Fast startup/re-hydration is critical
- State store writes are cheap (e.g., in-memory, Redis)
- Horizontal scaling where multiple instances handle the same aggregate

**Use Deferred when:**
- High command volume (many commands per second)
- Aggregate has long event history (re-hydration is already slow)
- State store writes are expensive (e.g., PostgreSQL)
- Throughput is prioritized over startup time
- Single-instance or sharded deployment model

## 3. API Design

### 3.1 StateWriteMode Enum

```rust
/// Controls when aggregate state is persisted to the state store.
///
/// Since events are the source of truth and always persisted immediately,
/// state writes are purely an optimization. Deferring them can improve
/// throughput without sacrificing durability.
///
/// # Deployment Considerations
///
/// `Deferred` mode maintains pending state in instance-local memory.
/// It provides performance benefits only when each aggregate is handled
/// by a single instance at a time:
///
/// - ✅ Single instance deployment
/// - ✅ Green-blue deployment (with graceful shutdown flush)
/// - ✅ Sharded by aggregate ID
/// - ✅ Leader election per aggregate
/// - ⚠️ Horizontal scaling (no benefit, use `Immediate`)
///
/// Correctness is always guaranteed via re-hydration from events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum StateWriteMode {
    /// Write state after every command.
    ///
    /// Use when:
    /// - State store writes are cheap (in-memory, Redis)
    /// - Fast re-hydration on startup is critical
    /// - Multiple instances may handle the same aggregate
    #[default]
    Immediate,
    
    /// Defer state writes until thresholds are crossed.
    ///
    /// State is written when either:
    /// - `event_threshold` events have been applied since last write, OR
    /// - `max_delay_ms` milliseconds have elapsed since last write
    ///
    /// On crash, more events may need to be replayed during re-hydration,
    /// but no data is lost since events are always persisted immediately.
    ///
    /// **Important:** Call [`flush_state()`] during graceful shutdown to
    /// persist pending state before the instance terminates.
    ///
    /// Use when:
    /// - State store writes are expensive (PostgreSQL)
    /// - High command throughput
    /// - Single-instance or sharded deployment
    Deferred {
        /// Number of events applied before persisting state.
        /// Default: 100
        event_threshold: u32,
        /// Maximum time (in milliseconds) before forcing a state write.
        /// Default: 30000 (30 seconds)
        max_delay_ms: u64,
    },
}
```

### 3.2 Helper Constructors

```rust
impl StateWriteMode {
    /// Creates a new `Deferred` state write mode with the given settings.
    ///
    /// # Arguments
    /// * `event_threshold` - Number of events before persisting state
    /// * `max_delay` - Maximum time before forcing a state write
    ///
    /// # Example
    /// ```
    /// use epoch_core::StateWriteMode;
    /// use std::time::Duration;
    ///
    /// let mode = StateWriteMode::deferred(50, Duration::from_secs(10));
    /// ```
    pub fn deferred(event_threshold: u32, max_delay: Duration) -> Self {
        Self::Deferred {
            event_threshold,
            max_delay_ms: max_delay.as_millis() as u64,
        }
    }

    /// Creates a `Deferred` state write mode with default settings.
    ///
    /// Defaults: `event_threshold = 100`, `max_delay = 30 seconds`
    pub fn deferred_default() -> Self {
        Self::Deferred {
            event_threshold: 100,
            max_delay_ms: 30_000,
        }
    }
}
```

## 4. Implementation Design

### 4.1 Tracking State for Deferred Writes

We need to track per-aggregate metadata for deferred state writes:

```rust
/// Tracks pending state write metadata for an aggregate.
#[derive(Debug, Clone)]
pub struct PendingStateWrite<S> {
    /// The state to be written
    pub state: S,
    /// Number of events applied since last state write
    pub events_since_write: u32,
    /// When the first event was applied since last state write
    pub first_event_time: Instant,
    /// The version of the state
    pub version: u64,
}
```

### 4.2 Decision Logic

```rust
fn should_write_state(
    pending: &PendingStateWrite<S>,
    mode: &StateWriteMode,
) -> bool {
    match mode {
        StateWriteMode::Immediate => true,
        StateWriteMode::Deferred { event_threshold, max_delay_ms } => {
            let max_delay = Duration::from_millis(*max_delay_ms);
            pending.events_since_write >= *event_threshold
                || pending.first_event_time.elapsed() >= max_delay
        }
    }
}
```

### 4.3 Where to Store Pending State

The challenge is that `Aggregate::handle` is stateless - it doesn't maintain state between calls. We have several options:

#### Option A: Aggregate-Level State (Recommended)

Add optional pending state tracking to the `Aggregate` trait implementation:

```rust
#[async_trait]
pub trait Aggregate<ED>: Projection<ED>
where
    // ... existing bounds ...
{
    // ... existing methods ...
    
    /// Returns the state write mode for this aggregate.
    /// Override to use deferred writes.
    fn state_write_mode(&self) -> StateWriteMode {
        StateWriteMode::Immediate
    }
    
    /// Returns a reference to the pending state tracker, if any.
    /// Used internally for deferred state writes.
    fn pending_state(&self) -> Option<&Mutex<Option<PendingStateWrite<Self::State>>>>;
}
```

Aggregates opting into deferred writes would hold a `Mutex<Option<PendingStateWrite<S>>>`.

#### Option B: State Store Integration

Extend `StateStoreBackend` to support deferred writes:

```rust
#[async_trait]
pub trait StateStoreBackend<S>: Send + Sync {
    // ... existing methods ...
    
    /// Marks state as pending write (for deferred mode).
    async fn mark_pending(&mut self, id: Uuid, state: S) -> Result<(), Self::Error>;
    
    /// Flushes all pending state writes.
    async fn flush_pending(&mut self) -> Result<(), Self::Error>;
}
```

#### Option C: External Coordinator

A separate `StateWriteCoordinator` that wraps aggregates and manages deferred writes.

**Recommendation:** Option A is cleanest - it keeps the batching logic close to the aggregate and doesn't require changes to the state store trait.

### 4.4 Modified `handle` Flow

```rust
async fn handle(&self, command: Command<...>) -> Result<...> {
    // 1-5: Same as current (get state, re-hydrate, handle command, apply events, store events)
    
    // 6. Conditional state persistence
    let events_applied = events.len() as u32;
    
    match self.state_write_mode() {
        StateWriteMode::Immediate => {
            // Current behavior: write immediately
            if let Some(ref state) = state {
                state_store.persist_state(state.get_id(), state.clone()).await?;
            } else {
                state_store.delete_state(state_id).await?;
            }
        }
        StateWriteMode::Deferred { .. } => {
            // Update pending state tracker
            if let Some(pending_mutex) = self.pending_state() {
                let mut pending_guard = pending_mutex.lock().await;
                
                match pending_guard.as_mut() {
                    Some(pending) => {
                        pending.state = state.clone();
                        pending.events_since_write += events_applied;
                        pending.version = new_state_version;
                    }
                    None => {
                        *pending_guard = state.clone().map(|s| PendingStateWrite {
                            state: s,
                            events_since_write: events_applied,
                            first_event_time: Instant::now(),
                            version: new_state_version,
                        });
                    }
                }
                
                // Check if we should flush
                if let Some(ref pending) = *pending_guard {
                    if should_write_state(pending, &self.state_write_mode()) {
                        state_store.persist_state(pending.state.get_id(), pending.state.clone()).await?;
                        *pending_guard = None;
                    }
                }
            }
        }
    }
    
    Ok(state)
}
```

### 4.5 Flush on State Request

When re-hydrating, if there's pending state that's more recent than what's in the store, we should use it:

```rust
// In handle(), when getting initial state:
let state = if let Some(pending_mutex) = self.pending_state() {
    let pending_guard = pending_mutex.lock().await;
    if let Some(ref pending) = *pending_guard {
        if pending.state.get_id() == state_id {
            // Use pending state instead of fetching from store
            Some(pending.state.clone())
        } else {
            state_store.get_state(state_id).await?
        }
    } else {
        state_store.get_state(state_id).await?
    }
} else {
    state_store.get_state(state_id).await?
};
```

### 4.6 Explicit Flush API

Add a method to force-flush pending state:

```rust
impl<ED> dyn Aggregate<ED> {
    /// Flushes any pending state to the state store.
    ///
    /// Call this before shutdown or when you need to ensure state is persisted.
    pub async fn flush_state(&self) -> Result<(), ...> {
        if let Some(pending_mutex) = self.pending_state() {
            let mut pending_guard = pending_mutex.lock().await;
            if let Some(pending) = pending_guard.take() {
                let mut state_store = self.get_state_store();
                state_store.persist_state(pending.state.get_id(), pending.state).await?;
            }
        }
        Ok(())
    }
}
```

### 4.7 Graceful Shutdown (Green-Blue Deployments)

For green-blue deployments, the old instance (green) should flush pending state before terminating. This ensures the new instance (blue) reads fresh state from the store.

**Recommended shutdown sequence:**

```rust
// In your application's shutdown handler
async fn graceful_shutdown(aggregates: Vec<Arc<dyn Aggregate<ED>>>) {
    // 1. Stop accepting new commands
    
    // 2. Wait for in-flight commands to complete
    
    // 3. Flush all pending state
    for aggregate in aggregates {
        if let Err(e) = aggregate.flush_state().await {
            log::error!("Failed to flush state during shutdown: {}", e);
        }
    }
    
    // 4. Proceed with shutdown
}
```

**If shutdown is not graceful (crash, kill -9):**
- Pending state is lost
- Events are preserved (always persisted immediately)
- New instance re-hydrates from events, reconstructing correct state
- Only cost is increased re-hydration time on startup

### 4.8 Handling Deletions

When state is deleted (command returns `None`), we need to handle this in deferred mode:

```rust
// Track that state should be deleted
enum PendingStateAction<S> {
    Write(PendingStateWrite<S>),
    Delete { id: Uuid, events_since_write: u32, first_event_time: Instant },
}
```

## 5. Files to Modify

1. **`epoch_core/src/aggregate.rs`**:
   - Add `StateWriteMode` enum
   - Add `PendingStateWrite` struct
   - Add `should_write_state()` helper
   - Add `state_write_mode()` method to `Aggregate` trait
   - Add `pending_state()` method to `Aggregate` trait
   - Add `flush_state()` helper function
   - Modify `handle()` to support deferred writes

2. **`epoch_core/src/lib.rs`**:
   - Re-export `StateWriteMode`

3. **`epoch_core/src/prelude.rs`** (if exists):
   - Add `StateWriteMode` to prelude

## 6. Testing Strategy

### 6.1 Unit Tests

1. **`state_write_mode_variants`**: Verify both variants exist and have correct defaults
2. **`state_write_mode_helper_methods`**: Test `deferred()` and `deferred_default()` constructors
3. **`should_write_on_event_threshold`**: Verify write triggers at event_threshold
4. **`should_write_on_max_delay`**: Verify write triggers at max_delay
5. **`should_not_write_prematurely`**: Verify no write before thresholds

### 6.2 Integration Tests

1. **`test_immediate_mode_writes_every_command`**:
   - Configure `Immediate` mode
   - Execute 5 commands
   - Verify state store has 5 writes

2. **`test_deferred_mode_batches_writes`**:
   - Configure `Deferred { event_threshold: 10, max_delay_ms: 60000 }`
   - Execute 3 commands producing 3 events each (9 total)
   - Verify state store has 0 writes
   - Execute 1 more command producing 2 events (11 total)
   - Verify state store has 1 write

3. **`test_deferred_mode_respects_max_delay`**:
   - Configure `Deferred { event_threshold: 100, max_delay_ms: 500 }`
   - Execute 1 command
   - Wait 600ms, execute another command
   - Verify state was written due to max_delay

4. **`test_deferred_mode_uses_pending_state`**:
   - Configure deferred mode
   - Execute command, state is pending (not written)
   - Execute another command on same aggregate
   - Verify re-hydration uses pending state, not stale store state

5. **`test_deferred_mode_flush_on_explicit_call`**:
   - Configure deferred mode
   - Execute commands, don't reach threshold
   - Call `flush_state()`
   - Verify state is persisted

6. **`test_durability_after_simulated_crash`**:
   - Configure deferred mode
   - Execute commands, events are persisted
   - Simulate crash (drop aggregate without flushing)
   - Create new aggregate, verify re-hydration reconstructs correct state from events

### 6.3 Performance Tests

1. **`bench_immediate_vs_deferred`**:
   - Execute 1000 commands with `Immediate` mode, measure time
   - Execute 1000 commands with `Deferred` mode, measure time
   - Assert deferred is significantly faster

## 7. Backward Compatibility

This is a **fully backward-compatible** addition:
- `StateWriteMode::Immediate` is the default
- Existing aggregates continue to work unchanged
- No changes to `StateStoreBackend` trait required
- Aggregates opt-in to deferred mode by implementing `state_write_mode()` and `pending_state()`

## 8. Future Considerations

### 8.1 Shared State Cache (Redis)

A future enhancement could introduce a shared state cache (e.g., Redis) that enables `Deferred` mode benefits in horizontally scaled deployments:

```
┌─────────────┐     ┌─────────────┐     ┌─────────────┐
│  Instance A │     │    Redis    │     │  PostgreSQL │
│  Instance B │────▶│   (cache)   │────▶│  (durable)  │
│  Instance C │     │   shared    │     │   storage   │
└─────────────┘     └─────────────┘     └─────────────┘
```

**Architecture evolution:**

```rust
/// Fast, shared cache for state (Redis, Memcached)
/// Always written immediately for cross-instance consistency.
pub trait StateCache<S>: Send + Sync {
    type Error: std::error::Error + Send + Sync;
    
    async fn get(&self, id: Uuid) -> Result<Option<S>, Self::Error>;
    async fn set(&self, id: Uuid, state: &S) -> Result<(), Self::Error>;
    async fn delete(&self, id: Uuid) -> Result<(), Self::Error>;
}

/// Durable storage for state (PostgreSQL, S3)
/// May be written less frequently as an optimization.
pub trait DurableStateStore<S>: Send + Sync {
    type Error: std::error::Error + Send + Sync;
    
    async fn get(&self, id: Uuid) -> Result<Option<S>, Self::Error>;
    async fn persist(&self, id: Uuid, state: &S) -> Result<(), Self::Error>;
    async fn delete(&self, id: Uuid) -> Result<(), Self::Error>;
}
```

**Modified flow with cache + durable store:**

```rust
async fn handle(&self, command: Command<...>) -> Result<...> {
    // 1. Try cache first, fall back to durable store
    let state = match self.cache.get(id).await? {
        Some(s) => Some(s),
        None => self.durable_store.get(id).await?,
    };
    
    // 2. Re-hydrate from events (unchanged)
    let state = self.re_hydrate(state, events_since).await?;
    
    // 3. Handle command, produce events (unchanged)
    let events = self.handle_command(&state, cmd).await?;
    
    // 4. Store events - ALWAYS immediate (unchanged)
    self.event_store.store_events(events).await?;
    
    // 5. Update cache - ALWAYS immediate (enables horizontal scaling)
    self.cache.set(id, &state).await?;
    
    // 6. Update durable store - per StateWriteMode
    if self.should_write_durable() {
        self.durable_store.persist(id, &state).await?;
    }
    
    Ok(state)
}
```

**Benefits of cache + durable store separation:**
- Cache is always fresh → horizontal scaling works
- Durable writes can be deferred → throughput improvement
- On cache eviction/miss, fall back to durable store + re-hydration
- Clear separation of concerns

**When to implement:** When horizontal scaling becomes a requirement and Redis (or similar) is added to the infrastructure.

### 8.2 Rename to DurableWriteMode

If/when the cache layer is introduced, consider renaming:
- `StateWriteMode` → `DurableWriteMode` (clearer that it controls durable storage, not cache)
- Or introduce `CacheWriteMode` separately (though cache should likely always be immediate)

### 8.3 Background Flush Task

For long-running applications, a background task could periodically check and flush pending states that have exceeded `max_delay`, rather than only checking during `handle()`.

### 8.4 Per-Aggregate Configuration

Allow different aggregates to have different thresholds based on their write patterns.

### 8.5 Metrics

Track metrics for deferred writes:
- Number of writes saved
- Average batch size
- Re-hydration time impact

## 9. Implementation Plan

### Phase 1: Core Types
1. Add `StateWriteMode` enum to `epoch_core/src/aggregate.rs`
2. Add helper constructors
3. Add `PendingStateWrite` struct
4. Add `should_write_state()` helper
5. Add unit tests

### Phase 2: Trait Extensions
1. Add `state_write_mode()` default method to `Aggregate` trait
2. Add `pending_state()` method to `Aggregate` trait
3. Add `flush_state()` helper function

### Phase 3: Handle Integration
1. Modify `Aggregate::handle()` to check `state_write_mode()`
2. Implement pending state tracking in `handle()`
3. Implement pending state usage during re-hydration
4. Handle deletion case

### Phase 4: Testing
1. Add integration tests
2. Add performance benchmarks

### Phase 5: Documentation
1. Update rustdoc
2. Update CHANGELOG.md
3. Update spec status to Implemented

## 10. Discussion Notes & Decision

### 10.1 Original Motivation

The initial motivation was to optimize a **seed script** that generates test data:

| Aggregate Type    | Entities       | Events         |
|-------------------|----------------|----------------|
| MachineDefinition | ~1             | ~1             |
| Organization      | ~16            | ~170           |
| JobConfiguration  | ~85            | ~85            |
| MachinePool       | ~16            | ~16            |
| Machine           | ~270           | ~810           |
| Job               | ~1,350         | ~5,000-6,000   |
| File              | ~600-800       | ~1,200-1,600   |
| **TOTAL**         | **~2,300-2,500** | **~7,000-9,000** |

The seed script takes **several minutes** to complete, which is slow for ~10k writes.

### 10.2 Analysis of Write Volume

Current write pattern per command:
- 1-2 event writes (most commands produce 1 event)
- 1 state write

Total for seed: ~9,500-11,500 individual DB writes.

Impact of `StateWriteMode::Deferred`:

| Metric | Current | With Deferred |
|--------|---------|---------------|
| State writes | ~2,500 | ~6 (one per aggregate type) |
| Event writes | ~8,000 | ~8,000 (unchanged) |
| **Total** | **~10,500** | **~8,006** |

**Reduction: ~24%** - meaningful but not transformative.

### 10.3 Root Cause: Lack of Transactions

The real bottleneck is **not** the number of state writes, but the **lack of transaction support**.

Without transactions, each write:
1. Sends query to PostgreSQL
2. PostgreSQL writes to WAL
3. PostgreSQL calls `fsync()` to ensure durability
4. Returns success

With ~10k individual writes = ~10k `fsync()` calls = slow.

**With transactions:**
```sql
-- Current: 10k fsyncs
INSERT INTO epoch_events (...);  -- fsync
INSERT INTO epoch_events (...);  -- fsync
-- ... repeat 10k times

-- With transaction: 1 fsync
BEGIN;
INSERT INTO epoch_events (...);  -- no fsync
INSERT INTO epoch_events (...);  -- no fsync
-- ... repeat 10k times
COMMIT;  -- single fsync
```

The difference can be **10-100x** for bulk operations.

### 10.4 Why StateWriteMode Doesn't Solve This

Even with `StateWriteMode::Deferred`:
- Still ~8,000 individual event writes
- Still ~8,000 fsync operations
- Seed would still take minutes

### 10.5 Recommended Alternative: Transaction Support

Transaction support would provide greater benefit:

**Level 1: Command-Level Transactions**
```rust
async fn handle(&self, command: Command<...>) -> Result<...> {
    let tx = pool.begin().await?;
    
    for event in events {
        store_event_with_tx(&tx, event).await?;
    }
    persist_state_with_tx(&tx, state).await?;
    
    tx.commit().await?;  // single fsync per command
}
```
- Reduces fsyncs from ~10k to ~7k-9k (one per command)
- Also provides atomicity (events + state committed together)

**Level 2: Batch Transaction API**
```rust
let mut tx = aggregate.begin_transaction().await?;

for cmd in seed_commands {
    tx.handle(cmd).await?;
}

tx.commit().await?;  // single fsync for entire batch
```
- Reduces fsyncs to 1 for entire seed
- Seed would complete in seconds instead of minutes

### 10.6 Decision

**This spec is deferred.** Transaction support should be implemented first as it:

1. Addresses the root cause (fsync per write)
2. Provides greater performance improvement (10-100x vs 24%)
3. Has additional benefits (atomicity, consistency)
4. Is required infrastructure regardless of `StateWriteMode`

After transaction support is implemented, `StateWriteMode::Deferred` may still be valuable for:
- Reducing state store writes in high-throughput production scenarios
- Use with future Redis cache layer (see Section 8.1)

A new spec for transaction support should be created.

### 10.7 Deployment Model Considerations (For Future Reference)

When/if this spec is implemented, the deployment model constraints remain:

| Deployment Model | Deferred Mode | Notes |
|------------------|---------------|-------|
| Single instance | ✅ Works | Ideal use case |
| Green-blue deployment | ✅ Works | Flush on graceful shutdown |
| Sharded by aggregate ID | ✅ Works | Each aggregate on one instance |
| Horizontal scaling | ⚠️ No benefit | Pending state not shared |

A future **shared cache layer (Redis)** would enable deferred durable writes even with horizontal scaling (see Section 8.1).
