# Specification: PgEventBus Reliability Improvements

**Spec ID:** 0002  
**Status:** ✅ Implemented  
**Created:** 2026-01-21  
**Completed:** 2026-01-26  

## Problem Statement

PostgreSQL `LISTEN/NOTIFY` provides low-latency notifications but has reliability issues:

1. **At-most-once delivery**: No delivery guarantees if subscriber disconnects
2. **No checkpoint tracking**: Projections can't resume from where they left off
3. **No catch-up mechanism**: Events during disconnection are lost forever
4. **Silent failures**: Projection errors logged but event lost
5. **No global ordering**: Only per-stream ordering via `stream_version`

### Failure Scenarios

| Scenario | Before | After |
|----------|--------|-------|
| Subscriber disconnects | Events lost | Catch-up on reconnect |
| Application restarts | Start from beginning | Resume from checkpoint |
| Projection error | Event skipped | Retry then DLQ |
| DB connection drops | Events lost | Catch-up after reconnect |

## Solution Overview

Implemented checkpoint-based catch-up with dead letter queue support:

1. **Global Sequence:** Added `global_sequence` column for cross-stream ordering
2. **Checkpoint Tracking:** Track last processed event per subscriber
3. **Catch-up on Subscribe:** Query missed events since checkpoint
4. **Catch-up on Reconnect:** Query missed events after connection loss
5. **Retry with Backoff:** Retry failed events with exponential backoff
6. **Dead Letter Queue:** Failed events after max retries go to DLQ
7. **Multi-Instance Coordination:** PostgreSQL advisory locks for leader election
8. **Batched Checkpoints:** Optional batching for high-throughput scenarios

## Key Design Decisions

### Global Sequence Implementation

Cannot use `ALTER TABLE ADD COLUMN BIGSERIAL`:
```sql
-- Create sequence manually
CREATE SEQUENCE IF NOT EXISTS events_global_sequence_seq;

-- Add column with default
ALTER TABLE events ADD COLUMN global_sequence BIGINT 
  DEFAULT nextval('events_global_sequence_seq');

-- Backfill existing rows
UPDATE events SET global_sequence = nextval('events_global_sequence_seq')
WHERE global_sequence IS NULL ORDER BY created_at, id;

-- Add NOT NULL constraint
ALTER TABLE events ALTER COLUMN global_sequence SET NOT NULL;
```

### Subscriber Identity as Pool Semantics

`subscriber_id()` identifies a **pool** of instances, not individual instances:
- All instances with same ID share the same checkpoint
- Events delivered exactly once per subscriber_id
- Multiple instances compete for event processing (via advisory locks)

Supports both single-instance and future horizontal scaling.

### Catch-up with Gap-Free Delivery

To prevent race between catch-up and real-time events:

1. Start NOTIFY listener first (buffer incoming events)
2. Read checkpoint
3. Query all events `WHERE global_sequence > checkpoint`
4. Process catch-up events, updating checkpoint
5. Process buffered events, deduplicating by global_sequence
6. Continue normal processing

### At-Least-Once Semantics

Checkpoint updated AFTER successful processing:
- On crash between processing and checkpoint: event redelivered
- **Projections must be idempotent**
- For exactly-once: projections can store own checkpoint in state

### Advisory Locks for Multi-Instance Coordination

Uses MD5 hash split into two int4 values for 64-bit key space:
```sql
SELECT pg_try_advisory_lock(
  ('x' || substr(md5($1), 1, 8))::bit(32)::int,
  ('x' || substr(md5($1), 9, 8))::bit(32)::int
)
```

**Why not hashtext?** 32-bit hashtext has ~1% collision at 10k subscribers.

**Behavior:**
- Instance attempts lock on `subscribe()`
- If acquired: becomes active processor
- If not acquired: logs info, another instance processing
- On crash: lock auto-released, failover to another instance

### DLQ Without Foreign Key

`event_id` in DLQ is intentionally NOT a foreign key:
- Allows event purging/archival without blocking DLQ
- DLQ entry remains valid even if source event deleted

## Database Schema

### Checkpoint Table
```sql
CREATE TABLE epoch_event_bus_checkpoints (
    subscriber_id VARCHAR(255) PRIMARY KEY,
    last_global_sequence BIGINT NOT NULL DEFAULT 0,
    last_event_id UUID,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

### Dead Letter Queue
```sql
CREATE TABLE epoch_event_bus_dlq (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    subscriber_id VARCHAR(255) NOT NULL,
    event_id UUID NOT NULL,
    global_sequence BIGINT NOT NULL,
    error_message TEXT,
    retry_count INT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_retry_at TIMESTAMPTZ,
    UNIQUE (subscriber_id, event_id)
);
```

## API Changes

### EventObserver Trait

Added required `subscriber_id()` method:
```rust
trait EventObserver<ED> {
    fn subscriber_id(&self) -> &str;
    async fn on_event(&self, event: Arc<Event<ED>>) -> Result<...>;
}
```

### Projection Trait

Added required `subscriber_id()` method:
```rust
trait Projection<ED>: EventApplicator<ED> + SubscriberId {
    fn subscriber_id(&self) -> &str;
    // ... existing methods
}
```

### Configuration

```rust
pub struct ReliableDeliveryConfig {
    pub max_retries: u32,                    // Default: 3
    pub initial_retry_delay: Duration,       // Default: 1s
    pub max_retry_delay: Duration,           // Default: 60s
    pub catch_up_batch_size: u32,           // Default: 100
    pub checkpoint_mode: CheckpointMode,     // Default: Synchronous
    pub instance_mode: InstanceMode,         // Default: Coordinated
}

pub enum CheckpointMode {
    Synchronous,  // Update after every event
    Batched { batch_size: u32, max_delay_ms: u64 },
}

pub enum InstanceMode {
    SingleInstance,  // No advisory locks
    Coordinated,     // Use advisory locks
}
```

## Alternatives Considered

### Separate SubscriberIdentity Trait
**Rejected:** Every observer needs an ID anyway, adds complexity

### Separate subscribe_reliable() Method
**Rejected:** All subscribers should have reliable delivery

### DLQ Management on PgEventBus
**Rejected:** Operational concern, handle via direct DB or external tooling

### External Message Queue (Kafka/RabbitMQ)
**Rejected:** Adds operational complexity, epoch aims for PostgreSQL-only simplicity

### Polling Only (No NOTIFY)
**Rejected:** Higher latency and database load

### Foreign Key on DLQ event_id
**Rejected:** Would block event purging/archival

## Breaking Changes

- `Event` struct gained `global_sequence: Option<u64>` field
- `EventObserver` trait requires `subscriber_id()` method
- `Projection` trait requires `subscriber_id()` method
- Sagas must implement `EventObserver::subscriber_id()`

Migration handled via `SubscriberId` derive macro and migrations on database.

## Future Enhancements

### Horizontal Scaling (Outbox Pattern)

Current advisory lock approach: only one instance processes per subscriber_id.

Future: Replace with outbox table + `SKIP LOCKED` for true horizontal scaling:

```sql
CREATE TABLE event_bus_outbox (
    id BIGSERIAL PRIMARY KEY,
    subscriber_pool VARCHAR(255) NOT NULL,
    global_sequence BIGINT NOT NULL,
    event_id UUID NOT NULL,
    status VARCHAR(20) DEFAULT 'pending',
    locked_by VARCHAR(255),
    ...
);

-- Workers claim events
UPDATE event_bus_outbox
SET status = 'processing', locked_by = $2
WHERE id = (
    SELECT id FROM event_bus_outbox
    WHERE subscriber_pool = $1 AND status = 'pending'
    ORDER BY global_sequence
    LIMIT 1
    FOR UPDATE SKIP LOCKED
)
```

Migration path is non-breaking:
- `subscriber_id()` already has "pool" semantics
- Checkpoint table unchanged
- Internal dispatch refactored

Most use cases don't need this - scale by running different projections on different instances.
