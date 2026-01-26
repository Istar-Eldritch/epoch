# Specification: PgEventBus Reliability Improvements

**Status:** Implemented  
**Created:** 2026-01-21  
**Updated:** 2026-01-26  
**Author:** AI Agent

## Implementation Notes

All core reliability features have been implemented:
- ✅ Checkpoint tracking and catch-up mechanism
- ✅ Retry with exponential backoff and DLQ
- ✅ Event buffering during catch-up to prevent race conditions
- ✅ Global sequence number with NOT NULL constraint
- ✅ Multi-instance coordination via advisory locks (`InstanceMode::Coordinated`)
- ✅ Batched checkpointing mode (`CheckpointMode::Batched`)  

## 1. Problem Statement

The current `PgEventBus` implementation uses PostgreSQL's `LISTEN/NOTIFY` mechanism for real-time event propagation. While this provides low-latency notifications, it has several reliability issues that can lead to **data loss**:

### 1.1 Current Issues

1. **At-most-once delivery**: PostgreSQL `NOTIFY` provides no delivery guarantees. If a subscriber is disconnected when an event is published, that event is lost forever for that subscriber.

2. **No checkpoint tracking**: Projections have no way to know which events they've successfully processed. After a restart or reconnection, they cannot resume from where they left off.

3. **No catch-up mechanism**: When the listener reconnects after a disconnection, it simply starts listening for new notifications. Any events that occurred during the disconnection window are never delivered.

4. **Silent failure on projection errors**: When a projection fails to process an event, the error is logged but the event is lost (see `// TODO: Send event to DLQ. & Retry` in `event_bus.rs`).

5. **No global event ordering**: The current schema only has `stream_version` (per-stream ordering). There's no global sequence number to enable "read all events since X" across all streams.

### 1.2 Failure Scenarios

| Scenario | Current Behavior | Expected Behavior |
|----------|-----------------|-------------------|
| Subscriber disconnects for 5 seconds | Events during gap are lost | Events are processed after reconnect |
| Application restarts | All past events are lost | Resume from last checkpoint |
| Projection throws error | Event is skipped | Event is retried or sent to DLQ |
| Database connection drops | Events during reconnect are lost | Catch-up after reconnect |

## 2. Proposed Solution

Implement a **checkpoint-based catch-up mechanism** with dead letter queue support.

### 2.1 High-Level Design

```
┌─────────────────────────────────────────────────────────────────────┐
│                         PgEventBus                                   │
│  ┌─────────────────┐    ┌─────────────────┐    ┌─────────────────┐ │
│  │  NOTIFY Listener │    │  Catch-up Worker │    │  Checkpoint     │ │
│  │  (real-time)     │───▶│  (on reconnect)  │◀──▶│  Store          │ │
│  └─────────────────┘    └─────────────────┘    └─────────────────┘ │
│           │                      │                                   │
│           ▼                      ▼                                   │
│  ┌─────────────────────────────────────────────────────────────────┐│
│  │                    Event Dispatcher                              ││
│  │  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐        ││
│  │  │Projection│  │Projection│  │  Saga    │  │   DLQ    │        ││
│  │  │    A     │  │    B     │  │          │  │ (failed) │        ││
│  │  └──────────┘  └──────────┘  └──────────┘  └──────────┘        ││
│  └─────────────────────────────────────────────────────────────────┘│
└─────────────────────────────────────────────────────────────────────┘
```

### 2.2 Key Components

#### 2.2.1 Global Event Sequence

Add a global sequence number to the events table for cross-stream ordering. See Section 4.1 for the correct migration strategy (note: `BIGSERIAL` cannot be added via `ALTER TABLE`).

This enables querying "all events since global_sequence X" regardless of stream.

#### 2.2.2 Checkpoint Store

Track the last successfully processed event for each subscriber:

```sql
CREATE TABLE event_bus_checkpoints (
    subscriber_id VARCHAR(255) PRIMARY KEY,
    last_global_sequence BIGINT NOT NULL,
    last_event_id UUID,  -- for debugging/auditing
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

#### 2.2.3 Subscriber Identity and Pools

Each subscriber (projection/saga) needs a unique, stable identifier. This identifier represents a **subscriber pool**—a logical group of application instances that collectively process events for a given subscriber.

```rust
#[async_trait]
pub trait EventObserver<ED>: Send + Sync
where
    ED: EventData + Send + Sync,
{
    /// Returns a unique, stable identifier for this subscriber pool.
    /// Used for checkpoint tracking across restarts.
    fn subscriber_id(&self) -> &str;
    
    /// Reacts to events published in an event bus.
    async fn on_event(
        &self,
        event: Arc<Event<ED>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}
```

**Subscriber Pool Semantics:**

The `subscriber_id` identifies a pool of workers that collectively process events:
- All application instances with the same `subscriber_id` share the same checkpoint
- Events are delivered exactly once per `subscriber_id` (not per instance)
- Multiple instances with the same `subscriber_id` compete for event processing

This design supports both single-instance deployments and future horizontal scaling (see Section 13).

#### 2.2.4 Catch-up Mechanism with Gap-Free Delivery

To prevent race conditions between catch-up completion and real-time listening, we use an **overlap strategy**:

1. **Start NOTIFY listener first** (before catch-up), buffering incoming events
2. Read the last checkpoint for this subscriber
3. Query all events with `global_sequence > last_checkpoint`
4. Process catch-up events, updating checkpoint after each
5. Process buffered real-time events, **deduplicating by `global_sequence`**
   - Skip any event where `global_sequence <= last_checkpoint`
6. Continue with normal real-time processing

This ensures no events are missed during the transition window.

**Catch-up failure handling:** If catch-up fails partway through, the subscriber resumes from the last successfully checkpointed `global_sequence` on the next attempt. Partial progress is preserved.

#### 2.2.5 Dead Letter Queue

For events that fail processing after retries:

```sql
CREATE TABLE event_bus_dlq (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    subscriber_id VARCHAR(255) NOT NULL,
    event_id UUID NOT NULL,  -- soft reference, no FK constraint
    global_sequence BIGINT NOT NULL,
    error_message TEXT,
    retry_count INT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_retry_at TIMESTAMPTZ,
    
    CONSTRAINT unique_subscriber_event UNIQUE (subscriber_id, event_id)
);
```

**Note:** The `event_id` is intentionally not a foreign key to avoid blocking event purging/archival operations. The DLQ entry remains valid even if the original event is deleted.

DLQ management (querying, retrying, removing events) is handled externally, not by `PgEventBus`. Recovery can be done via direct database queries or separate tooling.

#### 2.2.6 Checkpoint Management

`PgEventBus` manages checkpoints centrally. After a successful `on_event()` call returns, the bus updates the checkpoint for that subscriber. This applies to all observer types (projections, sagas, and custom observers).

**Atomicity considerations:**

- For projections using `PgStateStore`, the checkpoint update and state persistence happen in the **same database** but not the same transaction (different tables, potentially different connections).
- This means we use **at-least-once delivery semantics**: checkpoint is updated *after* successful processing.
- On crash between processing and checkpoint update, the event will be redelivered.
- **Projections should be idempotent** to handle duplicate deliveries gracefully.

For projections requiring exactly-once semantics, they can store their own checkpoint within their state and verify during `apply()`.

#### 2.2.7 Multi-Instance Coordination

When multiple application instances run simultaneously with the same `subscriber_id`, coordination is required to prevent duplicate processing. This implementation uses **PostgreSQL advisory locks** for leader election:

```sql
-- Each instance attempts to acquire an exclusive lock for its subscriber
-- Uses MD5 hash split into two int4 values for 64-bit key space (negligible collision risk)
SELECT pg_try_advisory_lock(
    ('x' || substr(md5($1), 1, 8))::bit(32)::int,
    ('x' || substr(md5($1), 9, 8))::bit(32)::int
)  -- $1 = subscriber_id
```

**Why dual-int4 instead of hashtext?** PostgreSQL's `hashtext()` returns a 32-bit integer, which has ~1% collision probability at 10,000 subscribers. Hash collisions cause silent failures where one subscriber stops processing entirely. The MD5-based approach provides a 64-bit key space with negligible collision risk even at 100,000+ subscribers.

**Behavior:**

1. On `subscribe()`, the instance attempts to acquire an advisory lock for the `subscriber_id`
2. **If lock acquired:** Instance becomes the active processor for this subscriber pool
3. **If lock not acquired:** Instance logs an info message and skips processing (another instance is handling it)
4. **On disconnect/shutdown:** Lock is automatically released, allowing another instance to take over

**Failover:**

- Advisory locks are tied to the database session
- If an instance crashes or loses connection, PostgreSQL automatically releases the lock
- Another instance will acquire the lock on its next attempt (during reconnection or periodic retry)

**Scaling pattern:**

To scale horizontally, run different projections on different instances rather than the same projection on multiple instances:

```
Instance A: projection:users, projection:orders
Instance B: projection:inventory, projection:reports
Instance C: saga:fulfillment, saga:notifications
```

For true horizontal scaling of a single subscriber across multiple instances, see Section 13 (Future: Horizontal Scaling).

## 3. API Changes

### 3.1 Updated `EventObserver` Trait

The `EventObserver` trait in `epoch_core` is updated to include subscriber identity:

```rust
/// Trait to define observers to the event bus.
///
/// Observers receive events wrapped in `Arc` for efficient sharing.
/// 
/// **Important:** Implementations should be idempotent to handle potential
/// duplicate deliveries during catch-up or after crashes.
#[async_trait]
pub trait EventObserver<ED>: Send + Sync
where
    ED: EventData + Send + Sync,
{
    /// Returns a unique, stable identifier for this subscriber pool.
    /// 
    /// This ID identifies a logical group of application instances that
    /// collectively process events. All instances sharing the same ID:
    /// - Share the same checkpoint (resume from same position)
    /// - Compete for event processing (only one processes each event)
    /// 
    /// The ID must be:
    /// - Unique across all subscriber pools in the system
    /// - Stable across application restarts
    /// - Deterministic (same logical subscriber = same ID)
    /// 
    /// Example: "projection:user-list" or "saga:order-fulfillment"
    fn subscriber_id(&self) -> &str;
    
    /// Reacts to events published in an event bus.
    ///
    /// The event is wrapped in `Arc` for efficient sharing across multiple observers.
    async fn on_event(
        &self,
        event: Arc<Event<ED>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}
```

### 3.2 Updated `Projection` Trait and Blanket Implementation

The `Projection` trait gains a required `subscriber_id()` method:

```rust
#[async_trait]
pub trait Projection<ED>
where
    ED: EventData + Send + Sync + 'static,
    <Self::EventType as TryFrom<ED>>::Error: Send + Sync,
{
    // ... existing associated types ...
    
    /// Returns a unique, stable identifier for this projection.
    /// Used for checkpoint tracking in reliable event delivery.
    /// 
    /// Example: "projection:user-list-v1"
    fn subscriber_id(&self) -> &str;
    
    // ... existing methods ...
}
```

The blanket `EventObserver` implementation for `Projection` delegates to this method:

```rust
#[async_trait]
impl<ED, T> EventObserver<ED> for T
where
    ED: EventData + Send + Sync + 'static,
    T: Projection<ED> + Send + Sync,
{
    fn subscriber_id(&self) -> &str {
        Projection::subscriber_id(self)
    }
    
    async fn on_event(
        &self,
        event: Arc<Event<ED>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.apply_and_store(&event).await?;
        Ok(())
    }
}
```

### 3.3 Updated `Saga` Trait

The `Saga` trait requires `EventObserver` as a supertrait but does **not** have a blanket implementation. Saga implementers must implement `EventObserver` directly, including `subscriber_id()`:

```rust
/// Example saga implementation
struct OrderFulfillmentSaga { /* ... */ }

#[async_trait]
impl<ED> EventObserver<ED> for OrderFulfillmentSaga
where
    ED: EventData + Send + Sync,
{
    fn subscriber_id(&self) -> &str {
        "saga:order-fulfillment"
    }
    
    async fn on_event(
        &self,
        event: Arc<Event<ED>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.process_event((*event).clone()).await?;
        Ok(())
    }
}
```

### 3.4 Updated `Event` Struct

Add `global_sequence` to the core `Event` struct as an optional field:

```rust
/// Event definition
#[derive(Debug, Clone, PartialEq)]
pub struct Event<D>
where
    D: EventData,
{
    // ... existing fields ...
    
    /// Global sequence number for cross-stream ordering.
    /// 
    /// This is `Some` when the event has been persisted to a store that
    /// supports global ordering (e.g., PostgreSQL). It is `None` for
    /// in-memory stores or before persistence.
    pub global_sequence: Option<u64>,
}
```

This allows observers to know their position in the global event stream and enables downstream systems to implement their own checkpointing if needed.

### 3.5 Configuration

```rust
/// Configuration for reliable event delivery.
#[derive(Debug, Clone)]
pub struct ReliableDeliveryConfig {
    /// Maximum number of retry attempts before sending to DLQ.
    /// Default: 3
    pub max_retries: u32,
    
    /// Initial delay between retries (doubles each attempt).
    /// Default: 1 second
    pub initial_retry_delay: Duration,
    
    /// Maximum delay between retries.
    /// Default: 60 seconds
    pub max_retry_delay: Duration,
    
    /// Batch size for catch-up queries.
    /// Default: 100
    pub catch_up_batch_size: u32,
    
    /// Checkpoint update mode.
    /// Default: Synchronous
    pub checkpoint_mode: CheckpointMode,
    
    /// Multi-instance coordination mode.
    /// Default: Coordinated (uses advisory locks)
    pub instance_mode: InstanceMode,
}

impl Default for ReliableDeliveryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_retry_delay: Duration::from_secs(1),
            max_retry_delay: Duration::from_secs(60),
            catch_up_batch_size: 100,
            checkpoint_mode: CheckpointMode::Synchronous,
            instance_mode: InstanceMode::Coordinated,
        }
    }
}

/// Controls how checkpoint updates are persisted.
#[derive(Debug, Clone, Default)]
pub enum CheckpointMode {
    /// Update checkpoint after every event (safest, slower).
    /// Guarantees at-least-once delivery with minimal duplicates on crash.
    #[default]
    Synchronous,
    
    /// Batch checkpoint updates for better performance.
    /// Allows a window of duplicate deliveries on crash.
    Batched {
        /// Number of events to process before updating checkpoint.
        batch_size: u32,
        /// Maximum time before forcing a checkpoint update.
        max_delay: Duration,
    },
}

/// Controls multi-instance coordination behavior.
#[derive(Debug, Clone, Default)]
pub enum InstanceMode {
    /// Use PostgreSQL advisory locks to ensure only one instance
    /// processes events for each subscriber. Recommended for multi-instance
    /// deployments. Provides automatic failover.
    #[default]
    Coordinated,
    
    /// Disable advisory locks. Use only when you guarantee a single
    /// instance per subscriber (e.g., single-instance deployment or
    /// external orchestration). Slightly lower overhead.
    SingleInstance,
}
```

## 4. Database Schema Changes

All schema changes are applied via `PgEventBus::initialize()` using idempotent DDL statements.

### 4.1 Events Table: Add Global Sequence

**Important:** PostgreSQL does not allow adding a `BIGSERIAL` column via `ALTER TABLE`. We must create the sequence manually:

```sql
-- Create sequence for global ordering (idempotent)
CREATE SEQUENCE IF NOT EXISTS events_global_sequence_seq;

-- Add column with default from sequence
-- Note: IF NOT EXISTS for ADD COLUMN requires PostgreSQL 9.6+
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.columns 
        WHERE table_name = 'events' AND column_name = 'global_sequence'
    ) THEN
        ALTER TABLE events ADD COLUMN global_sequence BIGINT;
    END IF;
END $$;

-- Set default for new rows
ALTER TABLE events ALTER COLUMN global_sequence SET DEFAULT nextval('events_global_sequence_seq');

-- Backfill existing rows that have NULL global_sequence
-- Orders by created_at to maintain rough temporal ordering for existing events
UPDATE events 
SET global_sequence = nextval('events_global_sequence_seq')
WHERE global_sequence IS NULL
ORDER BY created_at, id;

-- Now that all rows have values, add NOT NULL constraint
ALTER TABLE events ALTER COLUMN global_sequence SET NOT NULL;

-- Create index for efficient catch-up queries
CREATE INDEX IF NOT EXISTS idx_events_global_sequence ON events(global_sequence);

-- Make sequence owned by column for proper cleanup on table drop
ALTER SEQUENCE events_global_sequence_seq OWNED BY events.global_sequence;
```

**Update the NOTIFY trigger to include global_sequence:**

```sql
CREATE OR REPLACE FUNCTION epoch_pg_notify_event()
RETURNS TRIGGER AS $$
BEGIN
    PERFORM pg_notify(
        TG_ARGV[0],
        json_build_object(
            'id', NEW.id,
            'stream_id', NEW.stream_id,
            'stream_version', NEW.stream_version,
            'global_sequence', NEW.global_sequence,
            'event_type', NEW.event_type,
            'actor_id', NEW.actor_id,
            'purger_id', NEW.purger_id,
            'data', NEW.data,
            'created_at', NEW.created_at,
            'purged_at', NEW.purged_at
        )::text
    );
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;
```

### 4.2 Checkpoint Table

```sql
CREATE TABLE IF NOT EXISTS event_bus_checkpoints (
    subscriber_id VARCHAR(255) PRIMARY KEY,
    last_global_sequence BIGINT NOT NULL DEFAULT 0,
    last_event_id UUID,  -- for debugging/auditing
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

### 4.3 Dead Letter Queue

```sql
CREATE TABLE IF NOT EXISTS event_bus_dlq (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    subscriber_id VARCHAR(255) NOT NULL,
    event_id UUID NOT NULL,  -- intentionally no FK, see Section 2.2.5
    global_sequence BIGINT NOT NULL,
    error_message TEXT,
    retry_count INT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    last_retry_at TIMESTAMPTZ,
    
    CONSTRAINT unique_subscriber_event UNIQUE (subscriber_id, event_id)
);

CREATE INDEX IF NOT EXISTS idx_dlq_subscriber ON event_bus_dlq(subscriber_id);
CREATE INDEX IF NOT EXISTS idx_dlq_created_at ON event_bus_dlq(created_at);
```

## 5. Implementation Plan

### Phase 1: Core Trait Changes (Breaking)
1. Add `global_sequence: Option<u64>` field to `Event` struct in `epoch_core`
2. Update `EventBuilder` to support `global_sequence`
3. Add `subscriber_id()` method to `EventObserver` trait in `epoch_core`
4. Add `subscriber_id()` method to `Projection` trait in `epoch_core`
5. Update blanket `EventObserver` impl for `Projection` to delegate to `Projection::subscriber_id()`
6. Update all existing projection implementations to provide `subscriber_id()`
7. Update all existing saga implementations to implement `EventObserver::subscriber_id()`
8. Update example code and tests

### Phase 2: Global Sequence & Schema
1. Update `PgEventStore::initialize()` to run global_sequence migration (Section 4.1)
2. Update `PgEventBus::initialize()` to create checkpoint table
3. Update `PgEventBus::initialize()` to create DLQ table
4. Update `PgDBEvent` struct to include `global_sequence`
5. Update `PgEventStore` to populate `global_sequence` in returned events
6. Update NOTIFY trigger to include `global_sequence`

### Phase 3: Checkpoint Tracking & Multi-Instance Coordination
1. Add `ReliableDeliveryConfig` struct with `CheckpointMode` and `InstanceMode`
2. Add checkpoint read/write helper methods to `PgEventBus` (private)
3. Implement advisory lock acquisition on `subscribe()` (when `InstanceMode::Coordinated`)
4. Skip processing if lock not acquired (log info, allow another instance)
5. Update event dispatch loop to update checkpoint after successful `on_event()`
6. Add `CheckpointMode` support (synchronous initially)

### Phase 4: Catch-up & Reconnection
1. Add method to query events since a global sequence
2. Implement catch-up on subscriber registration:
   - Start NOTIFY listener and buffer events
   - Query events since last checkpoint
   - Process catch-up events with deduplication
   - Drain buffer with deduplication
3. Implement catch-up on reconnection (in listener loop)
4. Add batching for large catch-up windows

### Phase 5: Retry & DLQ
1. Implement retry logic with exponential backoff
2. Implement DLQ insertion after max retries
3. Add error context to DLQ entries

### Phase 6: Testing & Documentation
1. Unit tests for checkpoint logic
2. Unit tests for deduplication logic
3. Integration tests for catch-up scenarios
4. Integration tests for reconnection scenarios  
5. Integration tests for retry and DLQ
6. Integration tests for idempotency requirements
7. Update rustdoc documentation
8. Add migration guide for breaking changes

## 6. Backward Compatibility

### Breaking Changes

- **`Event` struct**: New `global_sequence: Option<u64>` field
- **`EventObserver` trait**: New required `subscriber_id()` method
- **`Projection` trait**: New required `subscriber_id()` method

All existing implementations of these traits must be updated.

### Migration Guide

1. **Update `Event` construction**: Add `global_sequence: None` to any manual `Event` construction (the builder handles this automatically).

2. **Implement `subscriber_id()` for projections**:
   ```rust
   impl Projection<MyEventData> for MyProjection {
       fn subscriber_id(&self) -> &str {
           "projection:my-projection-v1"
       }
       // ... rest of implementation
   }
   ```

3. **Implement `subscriber_id()` for custom `EventObserver` implementations**:
   ```rust
   impl EventObserver<MyEventData> for MyObserver {
       fn subscriber_id(&self) -> &str {
           "observer:my-observer"
       }
       // ... rest of implementation
   }
   ```

4. **Implement `EventObserver` for sagas** (sagas require manual implementation):
   ```rust
   impl EventObserver<MyEventData> for MySaga {
       fn subscriber_id(&self) -> &str {
           "saga:my-saga"
       }
       
       async fn on_event(&self, event: Arc<Event<MyEventData>>) -> Result<...> {
           self.process_event((*event).clone()).await?;
           Ok(())
       }
   }
   ```

### Non-Breaking Changes

- Global sequence column added to events table (existing events backfilled)
- New checkpoint and DLQ tables created
- Enhanced `subscribe()` behavior (catch-up, checkpointing)

## 7. Testing Strategy

### 7.1 Unit Tests
- Checkpoint read/write operations
- Retry delay calculations (exponential backoff)
- Batch processing logic
- Deduplication by global_sequence
- `CheckpointMode` behavior

### 7.2 Integration Tests
- Subscriber reconnection catches up on missed events
- Application restart resumes from checkpoint
- Failed events go to DLQ after max retries
- Multiple subscribers maintain independent checkpoints
- High-volume catch-up performance
- Catch-up with concurrent real-time events (overlap strategy)
- Idempotent projection handles duplicate delivery
- Multiple instances with same subscriber_id: only one processes events
- Instance failure triggers failover to another instance

### 7.3 Failure Injection Tests
- Kill connection during event processing → verify resume from checkpoint
- Simulate projection errors → verify retry then DLQ
- Database unavailable during checkpoint write → verify event redelivered
- Crash between processing and checkpoint → verify at-least-once delivery
- Kill active instance → verify another instance acquires advisory lock and continues

### 7.4 Migration Tests
- Fresh install creates all schema correctly
- Upgrade from previous version backfills global_sequence
- Existing events maintain temporal ordering after backfill

## 8. Observability

### 8.1 Logging
- INFO: Catch-up started/completed with event count
- INFO: Subscriber registered with checkpoint position
- INFO: Advisory lock acquired for subscriber (became active processor)
- INFO: Advisory lock not acquired for subscriber (another instance is active)
- INFO: Advisory lock released (on shutdown or failover)
- WARN: Event processing retry attempt
- ERROR: Event sent to DLQ
- DEBUG: Checkpoint updated

### 8.2 Future Metrics (Out of Scope)
The following metrics would be valuable for production monitoring but are out of scope for this specification:

- `epoch_catchup_events_total`: Counter of events processed during catch-up
- `epoch_catchup_duration_seconds`: Histogram of catch-up duration
- `epoch_checkpoint_lag`: Gauge of (current_head - last_checkpoint) per subscriber
- `epoch_dlq_size`: Gauge of DLQ entries per subscriber
- `epoch_retry_attempts_total`: Counter of retry attempts

## 9. Alternatives Considered

### 9.1 Separate `SubscriberIdentity` Trait
Keep `EventObserver` unchanged and add a separate `SubscriberIdentity` trait.
- Rejected: Adds complexity with multiple traits, every observer needs an ID anyway

### 9.2 Separate `subscribe_reliable()` Method
Keep existing `subscribe()` for at-most-once, add `subscribe_reliable()` for checkpointing.
- Rejected: All subscribers should have reliable delivery, no reason for two paths

### 9.3 DLQ Management Methods on `PgEventBus`
Add methods like `get_dlq_events()`, `retry_dlq_event()` to `PgEventBus`.
- Rejected: DLQ management is an operational concern, can be handled via direct DB access or separate tooling

### 9.4 External Message Queue (Kafka, RabbitMQ)
Use a dedicated message queue instead of PostgreSQL NOTIFY.
- Rejected: Adds operational complexity, epoch aims for simplicity with just PostgreSQL

### 9.5 Polling Only (No NOTIFY)
Remove NOTIFY entirely and rely on polling.
- Rejected: Higher latency, more database load for real-time use cases

### 9.6 Foreign Key on DLQ event_id
Add `REFERENCES events(id)` constraint on DLQ.
- Rejected: Would block event purging/archival; DLQ entries should remain even if source event is deleted

### 9.7 Separate `projection_id()` Method
Add a separate `projection_id()` to `Projection` trait distinct from `subscriber_id()`.
- Rejected: Unnecessary indirection; projections can implement `subscriber_id()` directly via the trait requirement

## 10. Success Metrics

- Zero events lost during normal operation
- Catch-up completes within acceptable time window (< 1 minute for 10k events)
- DLQ provides visibility into processing failures
- No regression in real-time latency for happy path (< 10ms added latency)
- Duplicate delivery rate < 0.1% under normal operation (only on crashes)

## 11. Security Considerations

- **Subscriber IDs**: Should not contain sensitive information as they are stored in plain text
- **DLQ error messages**: May contain stack traces; ensure no sensitive data leaks into error messages
- **Checkpoint table**: No sensitive data, but access should be restricted to the application

## 12. Future Enhancements (Out of Scope)

1. **Subscription handles**: Return a handle from `subscribe()` for unsubscription and status monitoring
2. **Metrics integration**: Built-in Prometheus/OpenTelemetry metrics
3. **DLQ retry API**: Methods on `PgEventBus` for programmatic DLQ management
4. **Exactly-once delivery**: Transactional outbox pattern for projections using PostgreSQL state stores
5. **Parallel catch-up**: Process catch-up events in parallel for faster recovery
6. **Horizontal scaling**: Distribute events across multiple instances of the same subscriber (see Section 13)

## 13. Future: Horizontal Scaling

This section documents the migration path to horizontal scaling, where multiple instances can process events for the same `subscriber_id` in parallel.

### 13.1 Current Limitation

The advisory lock approach (Section 2.2.7) ensures only **one instance** processes events for a given `subscriber_id` at a time. This provides:
- ✅ Simple implementation
- ✅ Automatic failover
- ✅ No duplicate processing
- ❌ No horizontal scaling per subscriber

### 13.2 Future Solution: Outbox with SKIP LOCKED

To enable horizontal scaling, replace direct NOTIFY dispatch with an **outbox table** and PostgreSQL's `SKIP LOCKED`:

```
┌─────────────┐     NOTIFY "wake up"        ┌─────────────┐
│  Event      │ ───────────────────────────▶│  Worker 1   │──┐
│  Inserted   │ ───────────────────────────▶│  Worker 2   │──┼── SELECT ... FOR UPDATE SKIP LOCKED
│             │ ───────────────────────────▶│  Worker 3   │──┘
└─────────────┘                             └─────────────┘
                                                   │
                                            Only ONE worker
                                            gets each event
```

**New schema:**

```sql
CREATE TABLE event_bus_outbox (
    id BIGSERIAL PRIMARY KEY,
    subscriber_pool VARCHAR(255) NOT NULL,
    global_sequence BIGINT NOT NULL,
    event_id UUID NOT NULL,
    status VARCHAR(20) NOT NULL DEFAULT 'pending',
    locked_by VARCHAR(255),
    locked_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    
    UNIQUE (subscriber_pool, global_sequence)
);
```

**Worker claim logic:**

```rust
async fn claim_next_event(&self) -> Option<OutboxEntry> {
    sqlx::query_as(r#"
        UPDATE event_bus_outbox
        SET status = 'processing', locked_by = $2, locked_at = NOW()
        WHERE id = (
            SELECT id FROM event_bus_outbox
            WHERE subscriber_pool = $1 AND status = 'pending'
            ORDER BY global_sequence
            LIMIT 1
            FOR UPDATE SKIP LOCKED
        )
        RETURNING *
    "#)
    .bind(&self.subscriber_id)
    .bind(&self.worker_id)
    .fetch_optional(&self.pool)
    .await
    .ok()
    .flatten()
}
```

### 13.3 Migration Path

The migration from advisory locks to outbox-based distribution is **non-breaking**:

| Aspect | Change Required |
|--------|-----------------|
| `EventObserver` trait | ✅ None |
| `subscriber_id()` semantics | ✅ None (already "pool" semantics) |
| Checkpoint table | ✅ None |
| DLQ table | ✅ None |
| User code | ✅ None |
| Internal dispatch | 🔶 Refactor (internal only) |
| New tables | 🔶 Additive |

**Estimated effort:** 2-3 days of implementation work.

### 13.4 When to Implement

Implement horizontal scaling when:
- A single projection cannot keep up with event throughput
- Catch-up times are unacceptably long
- High availability requires active-active processing

For most use cases, the advisory lock approach with failover is sufficient. Scale by running different projections on different instances rather than the same projection on multiple instances.
