# Specification: CheckpointMode::Batched

**Status:** Implemented  
**Created:** 2026-01-26  
**Author:** AI Agent  

## 1. Problem Statement

The current `CheckpointMode::Synchronous` implementation writes a checkpoint to the database after every successfully processed event. While this provides the strongest durability guarantees (minimal duplicates on crash), it introduces significant overhead:

- **Database round-trip per event**: Each checkpoint update requires a full database write
- **High-throughput bottleneck**: For projections processing thousands of events per second, checkpoint writes become a limiting factor
- **Increased catch-up time**: During catch-up of a large backlog, synchronous checkpoints dramatically slow down processing

The spec 0002 originally planned for a `CheckpointMode::Batched` variant but it was deferred. This spec defines the implementation.

## 2. Proposed Solution

Add `CheckpointMode::Batched` that buffers checkpoint updates and persists them:
1. After processing a configurable number of events (`batch_size`)
2. After a configurable time interval (`max_delay`) since the last checkpoint

This trades off a small window of potential duplicate deliveries on crash for significantly improved throughput.

### 2.1 Tradeoff Analysis

| Aspect | Synchronous | Batched |
|--------|-------------|---------|
| Checkpoint writes | 1 per event | 1 per batch |
| Throughput | Lower | Higher |
| Duplicates on crash | Minimal (0-1 events) | Up to `batch_size` events |
| Implementation complexity | Simple | Moderate |
| Memory usage | Minimal | Buffers last checkpoint |

### 2.2 Use Cases

**Use Synchronous when:**
- Processing critical events where duplicates are costly
- Low event volume (< 100 events/second)
- Projections that are not idempotent

**Use Batched when:**
- High event volume (1000+ events/second)
- Projections are idempotent (handle duplicates gracefully)
- Catch-up performance is critical
- The cost of reprocessing a batch on crash is acceptable

## 3. API Design

### 3.1 CheckpointMode Enum

```rust
/// Controls how checkpoint updates are persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum CheckpointMode {
    /// Update checkpoint after every event (safest, slower).
    /// Guarantees at-least-once delivery with minimal duplicates on crash.
    #[default]
    Synchronous,
    
    /// Batch checkpoint updates for better performance.
    /// Allows a window of duplicate deliveries on crash.
    Batched {
        /// Number of events to process before updating checkpoint.
        /// Checkpoint is also updated when this count is reached.
        /// Default: 100
        batch_size: u32,
        /// Maximum time (in milliseconds) before forcing a checkpoint update.
        /// Prevents unbounded delay when event rate is low.
        /// Default: 5000 (5 seconds)
        max_delay_ms: u64,
    },
}
```

**Note:** Using `u64` for `max_delay_ms` instead of `Duration` because enum variants with associated data must implement `Copy` to derive it on the enum, and `Duration` is `Copy`. Actually, `Duration` is `Copy`, but keeping it as `u64` for simplicity in configuration.

### 3.2 Helper Constructor

```rust
impl CheckpointMode {
    /// Creates a new `Batched` checkpoint mode with the given settings.
    pub fn batched(batch_size: u32, max_delay: Duration) -> Self {
        Self::Batched {
            batch_size,
            max_delay_ms: max_delay.as_millis() as u64,
        }
    }
    
    /// Creates a `Batched` checkpoint mode with default settings.
    /// Default: batch_size = 100, max_delay = 5 seconds
    pub fn batched_default() -> Self {
        Self::Batched {
            batch_size: 100,
            max_delay_ms: 5000,
        }
    }
}
```

## 4. Implementation Details

### 4.1 Checkpoint State

The listener task needs to track pending checkpoint updates per subscriber:

```rust
struct PendingCheckpoint {
    /// The global sequence to checkpoint
    global_sequence: u64,
    /// The event ID to checkpoint
    event_id: Uuid,
    /// Number of events processed since last checkpoint write
    events_since_checkpoint: u32,
    /// When the first event was processed since last checkpoint write
    first_event_time: Instant,
}
```

### 4.2 Checkpoint Decision Logic

After each successful event processing:

```rust
fn should_flush_checkpoint(
    pending: &PendingCheckpoint,
    config: &CheckpointMode,
) -> bool {
    match config {
        CheckpointMode::Synchronous => true,
        CheckpointMode::Batched { batch_size, max_delay_ms } => {
            let max_delay = Duration::from_millis(*max_delay_ms);
            pending.events_since_checkpoint >= *batch_size
                || pending.first_event_time.elapsed() >= max_delay
        }
    }
}
```

### 4.3 Listener Task Changes

The listener task's checkpoint handling changes from:

```rust
// Current (Synchronous only)
if succeeded {
    // Update checkpoint immediately
    sqlx::query(/* upsert checkpoint */)
        .execute(&checkpoint_pool)
        .await?;
}
```

To:

```rust
// New (Synchronous or Batched)
if succeeded || sent_to_dlq {
    // Update pending checkpoint state
    pending_checkpoints.entry(subscriber_id.clone())
        .or_insert_with(|| PendingCheckpoint::new(event_global_seq, event.id))
        .update(event_global_seq, event.id);
    
    if should_flush_checkpoint(&pending_checkpoints[&subscriber_id], &config.checkpoint_mode) {
        // Flush checkpoint to database
        flush_checkpoint(&checkpoint_pool, &subscriber_id, &pending_checkpoints[&subscriber_id]).await;
        pending_checkpoints.remove(&subscriber_id);
    }
}
```

### 4.4 Periodic Flush Timer

For `Batched` mode, we need a background timer to ensure checkpoints are flushed even when event rate is low:

```rust
// In the listener loop, add a timeout
tokio::select! {
    notification = listener.recv() => {
        // Process notification as before
    }
    _ = tokio::time::sleep(Duration::from_secs(1)) => {
        // Check all pending checkpoints for max_delay expiry
        flush_expired_checkpoints(&checkpoint_pool, &mut pending_checkpoints, &config).await;
    }
}
```

### 4.5 Flush on Shutdown

When the listener task is shutting down (e.g., due to connection error), all pending checkpoints must be flushed before reconnecting:

```rust
// Before dropping listener_option for reconnection
if !pending_checkpoints.is_empty() {
    flush_all_checkpoints(&checkpoint_pool, &pending_checkpoints).await;
    pending_checkpoints.clear();
}
```

### 4.6 Catch-up Behavior

During catch-up, batched checkpointing also applies:
- Process `batch_size` events before checkpointing
- This significantly speeds up large catch-up operations

### 4.7 In-Memory Cache Interaction

The existing in-memory checkpoint cache (`checkpoint_cache: HashMap<String, u64>`) is updated:
- **Immediately** after successful event processing (for deduplication)
- **Database write** is deferred according to `CheckpointMode`

This ensures deduplication remains effective even with batched checkpointing.

## 5. Files to Modify

1. **`epoch_pg/src/event_bus.rs`**:
   - Add `Batched` variant to `CheckpointMode` enum
   - Add helper methods on `CheckpointMode`
   - Add `PendingCheckpoint` struct
   - Modify listener task to support batched checkpointing
   - Modify catch-up to support batched checkpointing
   - Add unit tests for checkpoint decision logic

2. **`epoch_pg/src/lib.rs`**:
   - No changes needed (already re-exports `CheckpointMode`)

## 6. Testing Strategy

### 6.1 Unit Tests

1. **`checkpoint_mode_batched_exists`**: Verify the variant exists and differs from `Synchronous`
2. **`checkpoint_mode_batched_helper_methods`**: Test `batched()` and `batched_default()` constructors
3. **`should_flush_on_batch_size`**: Verify flush triggers at batch_size threshold
4. **`should_flush_on_max_delay`**: Verify flush triggers at max_delay threshold
5. **`should_not_flush_prematurely`**: Verify no flush before thresholds

### 6.2 Integration Tests

1. **`test_batched_checkpoint_flushes_at_batch_size`**:
   - Configure `Batched { batch_size: 5, max_delay_ms: 60000 }`
   - Publish 4 events → checkpoint should NOT be written
   - Publish 5th event → checkpoint SHOULD be written
   - Verify checkpoint value in database

2. **`test_batched_checkpoint_flushes_at_max_delay`**:
   - Configure `Batched { batch_size: 100, max_delay_ms: 500 }`
   - Publish 3 events
   - Wait 600ms
   - Verify checkpoint was written despite not reaching batch_size

3. **`test_batched_checkpoint_on_reconnection`**:
   - Configure batched mode
   - Publish events but don't reach batch threshold
   - Simulate connection loss
   - Verify checkpoint was flushed before reconnection
   - Verify resumption from correct position

4. **`test_batched_catchup_performance`**:
   - Insert 1000 events directly into database
   - Subscribe with `Synchronous` mode, measure catch-up time
   - Subscribe with `Batched` mode, measure catch-up time
   - Assert batched mode is significantly faster (at least 2x)

5. **`test_batched_deduplication_still_works`**:
   - Configure batched mode
   - Publish events
   - Verify deduplication works correctly (in-memory cache updated immediately)

## 7. Backward Compatibility

This is a **backward-compatible** addition:
- `CheckpointMode::Synchronous` remains the default
- Existing code continues to work unchanged
- New users can opt-in to `Batched` mode for performance

## 8. Future Considerations

### 8.1 Transactional Batching
For projections using PostgreSQL state stores, a future enhancement could batch the checkpoint update in the same transaction as the projection state update, providing stronger consistency guarantees.

### 8.2 Adaptive Batching
Automatically adjust batch size based on observed throughput and latency to optimize the tradeoff dynamically.

## 9. Implementation Plan

### Phase 1: Core Implementation
1. Add `Batched` variant to `CheckpointMode`
2. Add helper methods (`batched()`, `batched_default()`)
3. Add `PendingCheckpoint` struct
4. Add `should_flush_checkpoint()` helper function
5. Add unit tests for the above

### Phase 2: Listener Integration
1. Modify listener task to use `PendingCheckpoint` tracking
2. Implement periodic flush timer with `tokio::select!`
3. Implement flush on reconnection
4. Add integration tests

### Phase 3: Catch-up Integration
1. Modify catch-up loop to support batched checkpointing
2. Add catch-up performance test

### Phase 4: Documentation
1. Update rustdoc on `CheckpointMode`
2. Update CHANGELOG.md
3. Update spec status to Implemented
