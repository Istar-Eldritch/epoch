# Implementation Plan: PgEventBus Reliability Improvements

**Specification:** [0002-pg-event-bus-reliability.md](./0002-pg-event-bus-reliability.md)  
**Created:** 2026-01-21  
**Status:** Pending Approval

This document outlines a phased, TDD-based implementation plan for the PgEventBus reliability improvements.

---

## Overview

The implementation is divided into **6 phases**, each with clear deliverables and testable milestones. Each phase follows TDD principles: write failing tests first, implement to pass tests, then refactor.

**Estimated Total Effort:** 5-7 days

---

## Phase 1: Core Trait Changes (Breaking)

**Goal:** Add `global_sequence` to `Event` and `subscriber_id()` to `EventObserver` and `Projection` traits.

**Estimated Effort:** 1-1.5 days

### Step 1.1: Add `global_sequence` to `Event` struct

**Files to modify:**
- `epoch_core/src/event.rs`

**Changes:**
1. Add `global_sequence: Option<u64>` field to `Event<D>` struct
2. Update `EventBuilder<D>` with `global_sequence: Option<u64>` field  
3. Add `global_sequence()` builder method
4. Update `build()` to include `global_sequence`
5. Update `From<Event<D>> for EventBuilder<D>` to transfer `global_sequence`
6. Update `to_subset_event()` and `to_superset_event()` to preserve `global_sequence`

**Tests (TDD - write first):**
```rust
// epoch_core/src/event.rs - add to existing tests or create new test module

#[test]
fn event_builder_with_global_sequence() {
    let event = Event::<TestEventData>::builder()
        .stream_id(Uuid::new_v4())
        .event_type("Test".to_string())
        .global_sequence(42)
        .build()
        .unwrap();
    assert_eq!(event.global_sequence, Some(42));
}

#[test]
fn event_builder_without_global_sequence_defaults_to_none() {
    let event = Event::<TestEventData>::builder()
        .stream_id(Uuid::new_v4())
        .event_type("Test".to_string())
        .build()
        .unwrap();
    assert_eq!(event.global_sequence, None);
}

#[test]
fn event_into_builder_preserves_global_sequence() {
    let original = Event { /* ... */ global_sequence: Some(100) /* ... */ };
    let rebuilt = original.into_builder().build().unwrap();
    assert_eq!(rebuilt.global_sequence, Some(100));
}

#[test]
fn to_subset_event_preserves_global_sequence() {
    // Test that to_subset_event() transfers global_sequence
}

#[test]
fn to_superset_event_preserves_global_sequence() {
    // Test that to_superset_event() transfers global_sequence
}
```

### Step 1.2: Add `subscriber_id()` to `EventObserver` trait

**Files to modify:**
- `epoch_core/src/event_store.rs`

**Changes:**
1. Add `fn subscriber_id(&self) -> &str;` method to `EventObserver` trait
2. Update rustdoc with explanation of subscriber pool semantics

**Tests (TDD):**
```rust
// Tests will fail to compile until all EventObserver implementations provide subscriber_id()
// This is expected - move to Step 1.3 to fix
```

### Step 1.3: Add `subscriber_id()` to `Projection` trait and update blanket impl

**Files to modify:**
- `epoch_core/src/projection.rs`

**Changes:**
1. Add `fn subscriber_id(&self) -> &str;` method to `Projection` trait
2. Update blanket `impl<ED, T> EventObserver<ED> for T where T: Projection<ED>` to delegate

**Tests (TDD):**
```rust
#[test]
fn projection_subscriber_id_is_available_via_event_observer() {
    struct TestProj;
    impl Projection<TestEventData> for TestProj {
        fn subscriber_id(&self) -> &str { "projection:test" }
        // ... other impls
    }
    
    let observer: &dyn EventObserver<TestEventData> = &TestProj;
    assert_eq!(observer.subscriber_id(), "projection:test");
}
```

### Step 1.4: Update existing test projections to implement `subscriber_id()`

**Files to modify:**
- `epoch_pg/tests/pgeventbus_integration_tests.rs`
- `epoch_pg/tests/pgeventstore_integration_tests.rs`
- `epoch_pg/tests/pgstatestorage_integration_tests.rs`
- Any other test files with Projection implementations
- `epoch/examples/hello-world.rs` (if exists with projections)

**Changes:**
1. Add `subscriber_id()` implementation to all test projections
2. Follow naming convention: `"projection:<name>"` for projections, `"saga:<name>"` for sagas

### Step 1.5: Verify compilation and run all tests

**Commands:**
```bash
cargo build --all
cargo test --all
cargo clippy --all -- -D warnings
```

**Milestone:** All code compiles, all existing tests pass. `subscriber_id()` is required on all observers.

---

## Phase 2: Global Sequence & Schema

**Goal:** Add global sequence column to events table, update PgEventStore to populate it.

**Estimated Effort:** 1 day

### Step 2.1: Update `PgDBEvent` struct

**Files to modify:**
- `epoch_pg/src/event_store.rs`

**Changes:**
1. Add `global_sequence: Option<i64>` field to `PgDBEvent` struct

**Tests (TDD):**
```rust
#[test]
fn pg_db_event_serialization_with_global_sequence() {
    let db_event = PgDBEvent {
        // ... other fields
        global_sequence: Some(123),
    };
    let json = serde_json::to_string(&db_event).unwrap();
    let parsed: PgDBEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.global_sequence, Some(123));
}
```

### Step 2.2: Update `PgEventStore::initialize()` for global sequence migration

**Files to modify:**
- `epoch_pg/src/event_store.rs`

**Changes:**
1. Add SQL migration to create `events_global_sequence_seq` sequence
2. Add column if not exists with conditional check
3. Set default from sequence
4. Backfill existing rows (order by `created_at`, `id`)
5. Add NOT NULL constraint after backfill
6. Create index on `global_sequence`
7. Set sequence ownership

**Tests (TDD):**
```rust
#[tokio::test]
async fn test_initialize_creates_global_sequence_column() {
    // Setup fresh database
    // Call initialize()
    // Verify column exists via information_schema query
}

#[tokio::test]
async fn test_initialize_is_idempotent() {
    // Setup, call initialize() twice
    // Should not error on second call
}

#[tokio::test]
async fn test_backfill_assigns_global_sequence_to_existing_events() {
    // Insert events without global_sequence (simulate pre-migration)
    // Call initialize()
    // Verify all events have non-null global_sequence
    // Verify ordering matches created_at
}
```

### Step 2.3: Update `PgEventStore::store_event()` to use RETURNING

**Files to modify:**
- `epoch_pg/src/event_store.rs`

**Changes:**
1. Modify INSERT query to use `RETURNING global_sequence`
2. Populate `global_sequence` on the event before publishing to bus

**Tests (TDD):**
```rust
#[tokio::test]
async fn test_store_event_assigns_global_sequence() {
    let event = new_event(stream_id, 1, "value");
    assert_eq!(event.global_sequence, None);
    
    event_store.store_event(event.clone()).await.unwrap();
    
    // Read back and verify global_sequence is assigned
    let events: Vec<_> = event_store.read_events(stream_id).await.unwrap().collect().await;
    assert!(events[0].global_sequence.is_some());
}

#[tokio::test]
async fn test_global_sequence_is_monotonically_increasing() {
    // Store 3 events across 2 different streams
    // Verify global_sequence values are strictly increasing
}
```

### Step 2.4: Update `PgEventStore::read_events()` to populate `global_sequence`

**Files to modify:**
- `epoch_pg/src/event_store.rs`

**Changes:**
1. Add `global_sequence` to SELECT query
2. Set `global_sequence` on returned Event

**Tests (TDD):**
```rust
#[tokio::test]
async fn test_read_events_includes_global_sequence() {
    // Store event
    // Read back
    // Verify global_sequence is populated
}
```

### Step 2.5: Update NOTIFY trigger to include `global_sequence`

**Files to modify:**
- `epoch_pg/src/event_bus.rs`

**Changes:**
1. Update `epoch_pg_notify_event()` function to include `'global_sequence', NEW.global_sequence` in JSON
2. Update event parsing in listener to extract `global_sequence`

**Tests (TDD):**
```rust
#[tokio::test]
async fn test_event_bus_notification_includes_global_sequence() {
    // Subscribe a test observer that captures global_sequence
    // Store event
    // Verify received event has global_sequence set
}
```

**Milestone:** Events have global sequence assigned on insert, preserved through read and bus propagation.

---

## Phase 3: Checkpoint Tracking

**Goal:** Create checkpoint table and implement checkpoint read/write operations.

**Estimated Effort:** 0.5-1 day

### Step 3.1: Create checkpoint table schema

**Files to modify:**
- `epoch_pg/src/event_bus.rs`

**Changes:**
1. Add checkpoint table creation to `PgEventBus::initialize()`:
```sql
CREATE TABLE IF NOT EXISTS event_bus_checkpoints (
    subscriber_id VARCHAR(255) PRIMARY KEY,
    last_global_sequence BIGINT NOT NULL DEFAULT 0,
    last_event_id UUID,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
```

**Tests (TDD):**
```rust
#[tokio::test]
async fn test_initialize_creates_checkpoint_table() {
    // Call initialize()
    // Verify table exists
}
```

### Step 3.2: Add `ReliableDeliveryConfig` struct

**Files to modify:**
- `epoch_pg/src/event_bus.rs` (or new `epoch_pg/src/config.rs`)

**Changes:**
1. Create `ReliableDeliveryConfig` struct with defaults
2. Create `CheckpointMode` enum (Synchronous, Batched)
3. Create `InstanceMode` enum (Coordinated, SingleInstance)
4. Update `PgEventBus` to accept optional config

**Tests (TDD):**
```rust
#[test]
fn reliable_delivery_config_has_sensible_defaults() {
    let config = ReliableDeliveryConfig::default();
    assert_eq!(config.max_retries, 3);
    assert_eq!(config.initial_retry_delay, Duration::from_secs(1));
    // ... etc
}
```

### Step 3.3: Implement checkpoint helper methods

**Files to modify:**
- `epoch_pg/src/event_bus.rs`

**Changes:**
1. Add private `get_checkpoint(&self, subscriber_id: &str) -> Result<Option<u64>>` method
2. Add private `update_checkpoint(&self, subscriber_id: &str, global_sequence: u64, event_id: Uuid) -> Result<()>` method

**Tests (TDD):**
```rust
#[tokio::test]
async fn test_checkpoint_read_returns_none_for_new_subscriber() {
    // No checkpoint exists
    // get_checkpoint returns None or 0
}

#[tokio::test]
async fn test_checkpoint_write_and_read_roundtrip() {
    // Write checkpoint for subscriber "test:sub"
    // Read it back
    // Verify values match
}

#[tokio::test]
async fn test_checkpoint_update_is_upsert() {
    // Write checkpoint
    // Update with higher value
    // Read back, verify updated
}
```

### Step 3.4: Update event dispatch to checkpoint after successful processing

**Files to modify:**
- `epoch_pg/src/event_bus.rs`

**Changes:**
1. After successful `projection_guard.on_event()`, call `update_checkpoint()`
2. Pass `subscriber_id` and `global_sequence` from the event

**Tests (TDD):**
```rust
#[tokio::test]
async fn test_checkpoint_updated_after_successful_event_processing() {
    // Subscribe projection with known subscriber_id
    // Store event
    // Wait for processing
    // Verify checkpoint was updated
}

#[tokio::test]
async fn test_checkpoint_not_updated_on_processing_failure() {
    // Subscribe failing projection
    // Store event
    // Wait for processing (expect failure)
    // Verify checkpoint was NOT updated
}
```

**Milestone:** Checkpoints are stored and updated per subscriber after successful event processing.

---

## Phase 4: Catch-up & Reconnection

**Goal:** Implement catch-up on subscribe and reconnection using overlap strategy.

**Estimated Effort:** 1-1.5 days

### Step 4.1: Add method to query events since global sequence

**Files to modify:**
- `epoch_pg/src/event_store.rs` (or `epoch_pg/src/event_bus.rs`)

**Changes:**
1. Add `read_all_events_since(&self, global_sequence: u64) -> Result<EventStream>` method
2. This queries ALL events (not per-stream) ordered by global_sequence

**Tests (TDD):**
```rust
#[tokio::test]
async fn test_read_all_events_since_returns_events_after_sequence() {
    // Store 5 events across different streams
    // Read since sequence 2
    // Verify returns events 3, 4, 5
}

#[tokio::test]
async fn test_read_all_events_since_with_zero_returns_all() {
    // Store events
    // Read since 0
    // Verify all events returned
}
```

### Step 4.2: Refactor listener loop to support buffering

**Files to modify:**
- `epoch_pg/src/event_bus.rs`

**Changes:**
1. Extract event processing into separate async function
2. Add ability to buffer events during catch-up phase
3. Implement deduplication by global_sequence (skip if `event.global_sequence <= last_checkpoint`)

**Tests (TDD):**
```rust
#[test]
fn deduplication_skips_events_below_checkpoint() {
    // Unit test for dedup logic
}
```

### Step 4.3: Implement catch-up on subscriber registration

**Files to modify:**
- `epoch_pg/src/event_bus.rs`

**Changes:**
1. Modify `subscribe()` to:
   a. Record the new observer with its `subscriber_id`
   b. Start listener and buffer real-time events
   c. Query checkpoint for subscriber
   d. Read all events since checkpoint
   e. Process catch-up events, updating checkpoint
   f. Drain buffered events with deduplication
2. Ensure catch-up happens before starting normal processing

**Tests (TDD):**
```rust
#[tokio::test]
async fn test_subscribe_catches_up_on_missed_events() {
    // Store 3 events BEFORE subscribing
    // Subscribe projection
    // Verify projection received all 3 events via catch-up
}

#[tokio::test]
async fn test_subscribe_deduplicates_events_during_catchup() {
    // Store event 1
    // Begin subscription (will start catch-up)
    // Store event 2 (arrives via NOTIFY during catch-up)
    // Verify each event received exactly once
}
```

### Step 4.4: Implement catch-up on reconnection

**Files to modify:**
- `epoch_pg/src/event_bus.rs`

**Changes:**
1. When connection is lost and re-established, trigger catch-up for each registered subscriber
2. Use same deduplication logic as initial subscription

**Tests (TDD):**
```rust
#[tokio::test]
async fn test_reconnection_catches_up_on_missed_events() {
    // Subscribe projection
    // Store event 1
    // Verify received
    // Simulate disconnection (how? may need to expose this for testing)
    // Store event 2 during "disconnection"
    // Restore connection
    // Verify event 2 is received via catch-up
}
```

**Note:** Testing reconnection scenarios may require:
- A method to force reconnection
- Or mocking the listener connection
- Or using testcontainers to actually kill/restart postgres

### Step 4.5: Add batching for large catch-up windows

**Files to modify:**
- `epoch_pg/src/event_bus.rs`

**Changes:**
1. Read catch-up events in batches of `config.catch_up_batch_size`
2. Update checkpoint after each batch to preserve progress

**Tests (TDD):**
```rust
#[tokio::test]
async fn test_catchup_with_many_events_uses_batching() {
    // Store 250 events (batch size = 100)
    // Subscribe projection
    // Verify all 250 events received
    // Verify multiple checkpoint updates occurred (can check updated_at progression)
}
```

**Milestone:** Subscribers catch up on missed events on subscribe and after reconnection. No events lost during normal reconnection scenarios.

---

## Phase 5: Retry & Dead Letter Queue

**Goal:** Implement retry with exponential backoff and DLQ for persistent failures.

**Estimated Effort:** 0.5-1 day

### Step 5.1: Create DLQ table schema

**Files to modify:**
- `epoch_pg/src/event_bus.rs`

**Changes:**
1. Add DLQ table creation to `PgEventBus::initialize()`:
```sql
CREATE TABLE IF NOT EXISTS event_bus_dlq (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    subscriber_id VARCHAR(255) NOT NULL,
    event_id UUID NOT NULL,
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

**Tests (TDD):**
```rust
#[tokio::test]
async fn test_initialize_creates_dlq_table() {
    // Call initialize()
    // Verify table exists
}
```

### Step 5.2: Implement retry logic with exponential backoff

**Files to modify:**
- `epoch_pg/src/event_bus.rs`

**Changes:**
1. On `on_event()` failure, calculate retry delay: `min(initial_delay * 2^attempt, max_delay)`
2. Sleep and retry up to `max_retries` times
3. Log each retry attempt at WARN level

**Tests (TDD):**
```rust
#[test]
fn retry_delay_calculation() {
    let config = ReliableDeliveryConfig {
        initial_retry_delay: Duration::from_secs(1),
        max_retry_delay: Duration::from_secs(60),
        ..Default::default()
    };
    assert_eq!(calculate_retry_delay(&config, 0), Duration::from_secs(1));
    assert_eq!(calculate_retry_delay(&config, 1), Duration::from_secs(2));
    assert_eq!(calculate_retry_delay(&config, 2), Duration::from_secs(4));
    assert_eq!(calculate_retry_delay(&config, 10), Duration::from_secs(60)); // capped
}

#[tokio::test]
async fn test_transient_failure_is_retried() {
    // Create projection that fails first 2 times, succeeds on 3rd
    // Store event
    // Verify event eventually processed successfully
    // Verify NOT in DLQ
}
```

### Step 5.3: Implement DLQ insertion after max retries

**Files to modify:**
- `epoch_pg/src/event_bus.rs`

**Changes:**
1. After `max_retries` failures, insert into DLQ table
2. Include `subscriber_id`, `event_id`, `global_sequence`, `error_message`, `retry_count`
3. Log at ERROR level
4. Continue processing next event (don't block)

**Tests (TDD):**
```rust
#[tokio::test]
async fn test_persistent_failure_goes_to_dlq() {
    // Create projection that always fails
    // Store event
    // Wait for retries to exhaust
    // Verify event is in DLQ with correct error message
}

#[tokio::test]
async fn test_dlq_entry_has_correct_metadata() {
    // Similar to above
    // Verify subscriber_id, event_id, global_sequence, retry_count
}

#[tokio::test]
async fn test_processing_continues_after_dlq_insertion() {
    // Create projection that fails on event 1, succeeds on event 2
    // Store both events
    // Verify event 1 in DLQ
    // Verify event 2 processed successfully
}
```

### Step 5.4: Update checkpoint logic for DLQ events

**Files to modify:**
- `epoch_pg/src/event_bus.rs`

**Changes:**
1. After DLQ insertion, still update checkpoint (event has been "handled", just not successfully)
2. This prevents infinite reprocessing of the same failing event

**Tests (TDD):**
```rust
#[tokio::test]
async fn test_checkpoint_advances_after_dlq_insertion() {
    // Failing projection
    // Store event
    // Wait for DLQ insertion
    // Verify checkpoint was updated to include this event
}
```

**Milestone:** Transient failures are retried; persistent failures go to DLQ; processing continues without blocking.

---

## Phase 6: Multi-Instance Coordination & Testing

**Goal:** Implement advisory locks for multi-instance coordination and comprehensive testing.

**Estimated Effort:** 1-1.5 days

### Step 6.1: Implement advisory lock acquisition on subscribe

**Files to modify:**
- `epoch_pg/src/event_bus.rs`

**Changes:**
1. When `InstanceMode::Coordinated`:
   - On `subscribe()`, attempt advisory lock using MD5-based dual-int4 approach:
     ```sql
     SELECT pg_try_advisory_lock(
         ('x' || substr(md5($1), 1, 8))::bit(32)::int,
         ('x' || substr(md5($1), 9, 8))::bit(32)::int
     )  -- $1 = subscriber_id
     ```
   - If acquired, proceed with normal processing
   - If not acquired, log INFO and skip event processing for this subscriber
2. Add helper function `acquire_subscriber_lock(subscriber_id: &str) -> Result<bool>`
3. Add helper function `release_subscriber_lock(subscriber_id: &str) -> Result<()>`

**Tests (TDD):**
```rust
#[tokio::test]
async fn test_single_instance_acquires_lock() {
    // Subscribe projection
    // Verify lock is acquired (can query pg_locks)
}

#[tokio::test]
async fn test_second_instance_skips_processing() {
    // Create two event buses with different pools but same channel
    // Subscribe same subscriber_id on both
    // Store event
    // Verify only one received the event
}

#[tokio::test]
async fn test_advisory_lock_uses_64bit_keyspace() {
    // Acquire lock for subscriber "test:a"
    // Query pg_locks to verify both classid and objid are set (dual-int4)
}
```

### Step 6.2: Implement automatic lock release on disconnect

**Files to modify:**
- `epoch_pg/src/event_bus.rs`

**Changes:**
1. Advisory locks are session-bound, so they auto-release on disconnect
2. Add explicit `pg_advisory_unlock()` on graceful shutdown (if possible)

**Tests (TDD):**
```rust
#[tokio::test]
async fn test_lock_released_on_disconnect() {
    // Acquire lock on pool1
    // Close pool1 connection
    // Verify lock is released (pool2 can acquire it)
}
```

### Step 6.3: Implement `InstanceMode::SingleInstance` (skip locking)

**Files to modify:**
- `epoch_pg/src/event_bus.rs`

**Changes:**
1. When `InstanceMode::SingleInstance`, skip advisory lock logic entirely
2. Useful for single-instance deployments or external orchestration

**Tests (TDD):**
```rust
#[tokio::test]
async fn test_single_instance_mode_skips_locking() {
    // Configure with SingleInstance mode
    // Subscribe projection
    // Verify no advisory lock was acquired (query pg_locks)
}
```

### Step 6.4: Comprehensive integration tests

**Files to modify:**
- `epoch_pg/tests/pgeventbus_integration_tests.rs` (expand)
- `epoch_pg/tests/reliability_integration_tests.rs` (new)

**New test scenarios:**

```rust
// Catch-up scenarios
#[tokio::test]
async fn test_subscriber_catches_up_after_late_subscription() { }

#[tokio::test]
async fn test_multiple_subscribers_maintain_independent_checkpoints() { }

#[tokio::test]
async fn test_high_volume_catchup_performance() { }

// Failure scenarios
#[tokio::test]
async fn test_crash_recovery_resumes_from_checkpoint() { }

#[tokio::test]
async fn test_idempotent_projection_handles_duplicate_delivery() { }

// Multi-instance scenarios
#[tokio::test]
async fn test_failover_to_second_instance() { }

#[tokio::test]
async fn test_different_projections_on_different_instances() { }
```

### Step 6.5: Update documentation

**Files to modify:**
- `epoch_core/src/event.rs` (rustdoc for `global_sequence`)
- `epoch_core/src/event_store.rs` (rustdoc for `subscriber_id()`)
- `epoch_core/src/projection.rs` (rustdoc for `subscriber_id()`)
- `epoch_pg/src/event_bus.rs` (module-level docs, config docs)
- `README.md` or `docs/` (migration guide)

**Changes:**
1. Document new `global_sequence` field
2. Document `subscriber_id()` requirements and best practices
3. Document `ReliableDeliveryConfig` options
4. Write migration guide for breaking changes
5. Add examples of idempotent projections

**Milestone:** Full reliability implementation complete with multi-instance support and comprehensive test coverage.

---

## Summary Table

| Phase | Focus | Breaking Changes | Estimated Effort |
|-------|-------|------------------|------------------|
| 1 | Core Trait Changes | Yes (`Event`, `EventObserver`, `Projection`) | 1-1.5 days |
| 2 | Global Sequence & Schema | No | 1 day |
| 3 | Checkpoint Tracking | No | 0.5-1 day |
| 4 | Catch-up & Reconnection | No | 1-1.5 days |
| 5 | Retry & DLQ | No | 0.5-1 day |
| 6 | Multi-Instance & Testing | No | 1-1.5 days |

**Total: 5-7 days**

---

## Dependencies Between Phases

```
Phase 1 (Core Traits)
    │
    └──► Phase 2 (Global Sequence)
              │
              └──► Phase 3 (Checkpoints)
                        │
                        ├──► Phase 4 (Catch-up)
                        │         │
                        │         └──► Phase 6 (Multi-Instance)
                        │
                        └──► Phase 5 (Retry/DLQ)
                                  │
                                  └──► Phase 6 (Testing)
```

Phases 4 and 5 can be worked on in parallel after Phase 3.

---

## Rollback Strategy

Each phase is designed to be independently releasable:

1. **Phase 1**: Breaking change, requires version bump. No rollback once released.
2. **Phase 2-6**: Additive/behavioral changes. Can be disabled via config if issues found.

For Phase 2+, the checkpoint and DLQ tables are additive. If issues are found:
- Set `InstanceMode::SingleInstance` to disable advisory locks
- Checkpoints can be cleared via `TRUNCATE event_bus_checkpoints`
- DLQ entries can be manually deleted

---

## Resolved Questions

1. **Batched Checkpoint Mode**: ✅ Yes, implement `CheckpointMode::Batched` in Phase 3.

2. **Reconnection Testing**: ✅ Use mocking approach for the listener connection.

3. **Idempotency Examples**: ✅ Yes, add dedicated examples showing how to make projections idempotent.

4. **Advisory Lock Hash Collisions**: ✅ Use dual-int4 approach with MD5 for 64-bit key space.
   
   **Rationale**: `hashtext()` returns int4 (32-bit), which has ~1% collision risk at 10,000 subscribers. Collisions cause silent failures where a subscriber stops processing. Using MD5 split into two int4 values provides effectively 64-bit key space with negligible collision risk.
   
   **Implementation**:
   ```sql
   SELECT pg_try_advisory_lock(
       ('x' || substr(md5($1), 1, 8))::bit(32)::int,
       ('x' || substr(md5($1), 9, 8))::bit(32)::int
   )  -- $1 = subscriber_id
   ```

---

## Approval Checklist

Before proceeding with implementation, please confirm:

- [ ] Phase breakdown is acceptable
- [ ] Test coverage is sufficient
- [ ] Breaking change approach in Phase 1 is acceptable
- [ ] Estimated effort aligns with timeline expectations
