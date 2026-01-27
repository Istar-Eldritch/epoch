# Specification: CheckpointMode::Batched

**Spec ID:** 0005  
**Status:** ✅ Implemented  
**Created:** 2026-01-26  
**Completed:** 2026-01-26  

## Problem Statement

`CheckpointMode::Synchronous` writes checkpoint after every event, causing:
- Database round-trip per event
- High-throughput bottleneck (thousands of events/second)
- Increased catch-up time during backlog processing

## Solution Overview

Added `CheckpointMode::Batched` that buffers checkpoint updates and persists:
1. After processing N events (`batch_size`)
2. After time interval (`max_delay_ms`) since last checkpoint

Trades small window of duplicate deliveries on crash for significantly improved throughput.

## Tradeoff Analysis

| Aspect | Synchronous | Batched |
|--------|-------------|---------|
| Checkpoint writes | 1 per event | 1 per batch |
| Throughput | Lower | Higher |
| Duplicates on crash | 0-1 events | Up to batch_size events |
| Memory | Minimal | Buffers last checkpoint |

## Key Design Decisions

### In-Memory Cache Updated Immediately

The in-memory checkpoint cache is updated **immediately** after successful processing:
- Database write deferred per `CheckpointMode`
- Deduplication remains effective even with batched checkpointing
- Prevents reprocessing events in same instance

### Checkpoint Decision Logic

```rust
fn should_flush_checkpoint(
    pending: &PendingCheckpoint,
    config: &CheckpointMode,
) -> bool {
    match config {
        CheckpointMode::Synchronous => true,
        CheckpointMode::Batched { batch_size, max_delay_ms } => {
            pending.events_since_checkpoint >= batch_size
                || pending.first_event_time.elapsed() >= Duration::from_millis(max_delay_ms)
        }
    }
}
```

### Flush on Shutdown

Before reconnection or shutdown, all pending checkpoints must be flushed to database.

### Catch-up Performance

During catch-up, batched checkpointing dramatically improves speed:
- Process 100 events before checkpointing (vs 100 individual writes)
- Essential for large backlog recovery

## API

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum CheckpointMode {
    /// Update after every event (safest, slower).
    #[default]
    Synchronous,
    
    /// Batch updates for performance.
    Batched {
        batch_size: u32,
        max_delay_ms: u64,
    },
}

impl CheckpointMode {
    pub fn batched(batch_size: u32, max_delay: Duration) -> Self {
        Self::Batched {
            batch_size,
            max_delay_ms: max_delay.as_millis() as u64,
        }
    }
    
    pub fn batched_default() -> Self {
        Self::Batched {
            batch_size: 100,
            max_delay_ms: 5000,
        }
    }
}
```

## Use Cases

**Use Synchronous when:**
- Processing critical events where duplicates costly
- Low volume (< 100 events/second)
- Projections not idempotent

**Use Batched when:**
- High volume (1000+ events/second)
- Projections are idempotent
- Catch-up performance critical
- Reprocessing batch on crash acceptable

## Periodic Flush Timer

For batched mode, background timer ensures checkpoints flushed even with low event rate:

```rust
tokio::select! {
    notification = listener.recv() => {
        // Process notification
    }
    _ = tokio::time::sleep(Duration::from_secs(1)) => {
        // Check all pending checkpoints for max_delay expiry
        flush_expired_checkpoints(...).await;
    }
}
```

## Backward Compatibility

Fully backward compatible:
- `Synchronous` is default
- Existing code unchanged
- Users opt-in to batched mode

## Future Considerations

### Transactional Batching
For projections using PostgreSQL state stores, batch checkpoint with state update in same transaction for stronger consistency.

### Adaptive Batching
Automatically adjust batch_size based on observed throughput and latency.
