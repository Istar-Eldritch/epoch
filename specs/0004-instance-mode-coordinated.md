# Specification: InstanceMode::Coordinated Implementation

**Status:** Implemented  
**Created:** 2026-01-26  
**Updated:** 2026-01-26  
**Author:** AI Agent  

## 1. Problem Statement

The `InstanceMode` enum currently only has a `SingleInstance` variant. The spec (0002-pg-event-bus-reliability.md) describes a `Coordinated` mode that uses PostgreSQL advisory locks to ensure only one instance processes events for each subscriber when multiple instances run simultaneously.

Currently, users must manually call the advisory lock helper methods (`try_acquire_subscriber_lock`, `release_subscriber_lock`) if they want multi-instance coordination. This is error-prone and doesn't integrate with the subscribe/listener lifecycle.

## 2. Proposed Solution

Add an `InstanceMode::Coordinated` variant that automatically handles advisory lock acquisition during `subscribe()` and in the event listener loop.

### 2.1 Behavior

When `InstanceMode::Coordinated` is configured:

1. **During `subscribe()`**: Attempt to acquire an advisory lock for the subscriber_id before performing catch-up
   - If lock acquired: Proceed with catch-up and register for real-time events
   - If lock not acquired: Log an info message and return `Ok(())` without subscribing (another instance is handling it)

2. **During listener reconnection**: After reconnecting the NOTIFY listener, re-attempt lock acquisition for each subscriber
   - If lock acquired: Resume processing for that subscriber
   - If lock not acquired: Skip that subscriber (another instance took over)

3. **Lock release**: Advisory locks are automatically released by PostgreSQL when the session ends (connection closed/dropped). No explicit release is needed.

### 2.2 API Changes

#### 2.2.1 InstanceMode Enum

```rust
/// Determines how multiple instances of the same subscriber coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum InstanceMode {
    /// No coordination between instances. Use when running a single instance
    /// or when external orchestration handles instance management.
    #[default]
    SingleInstance,
    
    /// Use PostgreSQL advisory locks to ensure only one instance
    /// processes events for each subscriber. Recommended for multi-instance
    /// deployments. Provides automatic failover when an instance dies.
    ///
    /// When enabled:
    /// - `subscribe()` attempts to acquire a lock before catch-up
    /// - If lock is not acquired, the subscribe call succeeds but the projection
    ///   is not registered (another instance is processing)
    /// - Locks are automatically released when the database connection closes
    Coordinated,
}
```

### 2.3 Implementation Details

#### 2.3.1 Subscribe Flow with Coordination

```
subscribe() with InstanceMode::Coordinated:
  1. Attempt to acquire advisory lock for subscriber_id
  2. If lock not acquired:
     - Log info: "Another instance is processing subscriber '{}'..."
     - Return Ok(()) without registering the projection
  3. If lock acquired:
     - Proceed with normal catch-up flow
     - Register for real-time events
     - Log info: "Acquired lock for subscriber '{}', now processing events"
```

#### 2.3.2 Listener Loop Considerations

The advisory lock is acquired on a database connection. When the connection drops (reconnection scenario), the lock is automatically released by PostgreSQL. Upon reconnection, another instance may acquire the lock.

For the current implementation, we will keep the listener loop simple:
- The lock is acquired during `subscribe()` on a dedicated connection
- If the connection is lost, the lock is released automatically
- Another instance may take over
- The original instance's listener will continue but its events will be ignored (deduplication via checkpoints handles this)

**Note**: A more sophisticated implementation could track lock state and re-acquire locks on reconnection, but this adds complexity. The current approach relies on:
- At-least-once delivery (checkpoints prevent skipping events)
- Idempotent projections (duplicates are safe)
- Natural failover (connection loss releases lock, another instance picks up)

#### 2.3.3 Lock Connection Management

Advisory locks are session-scoped. We need to hold the lock connection for the duration of the subscriber's lifetime. Options:

**Option A: Dedicated connection per subscriber** (Recommended)
- Acquire a dedicated connection from the pool for each subscriber
- Hold this connection for the lock
- Pro: Simple, lock lifetime tied to connection lifetime
- Con: Uses one connection per subscriber

**Option B: Single lock connection shared across subscribers**
- One connection holds all advisory locks
- Pro: Uses only one extra connection
- Con: If connection drops, all locks are released simultaneously

We'll use **Option A** for simplicity and isolation. Each subscriber that successfully acquires a lock will hold a dedicated connection.

### 2.4 Data Structure Changes

Add a field to track lock state in the event bus or per-subscription:

```rust
/// Tracks the advisory lock connection for a subscriber.
/// When this is dropped, the connection returns to the pool and the lock is released.
struct SubscriberLock {
    /// The connection holding the advisory lock (kept alive)
    _connection: sqlx::pool::PoolConnection<sqlx::Postgres>,
    subscriber_id: String,
}
```

**Alternative**: Instead of storing the connection, we can simply acquire the lock and let the pool manage the connection. The lock will be released when the connection is reused or closed. This is simpler but less deterministic about lock duration.

For this implementation, we'll use the simpler approach: acquire the lock at subscribe time and don't explicitly track the connection. The lock will naturally be released when:
- The application shuts down
- The connection is closed by the pool (idle timeout)
- The database session ends

This matches the behavior described in the original spec and is sufficient for most use cases.

## 3. Files to Modify

1. **`epoch_pg/src/event_bus.rs`**:
   - Add `InstanceMode::Coordinated` variant
   - Modify `subscribe()` to check instance mode and attempt lock acquisition
   - Update documentation

2. **`epoch_pg/tests/pgeventbus_integration_tests.rs`**:
   - Add tests for coordinated mode behavior

## 4. Implementation Plan

### Phase 1: Add Coordinated Variant (TDD)
1. Write test: `test_coordinated_mode_acquires_lock_on_subscribe`
2. Write test: `test_coordinated_mode_skips_subscribe_if_lock_held`
3. Add `Coordinated` variant to `InstanceMode`
4. Implement lock acquisition in `subscribe()` for coordinated mode
5. Run tests, verify passing

### Phase 2: Integration Testing
1. Write test: `test_coordinated_mode_allows_different_subscribers_on_same_instance`
2. Verify existing tests still pass
3. Run `cargo clippy` and `cargo fmt`

## 5. Test Cases

### 5.1 Unit Tests (in event_bus.rs)

```rust
#[test]
fn instance_mode_coordinated_exists() {
    let mode = InstanceMode::Coordinated;
    assert_ne!(mode, InstanceMode::SingleInstance);
}
```

### 5.2 Integration Tests

```rust
#[tokio::test]
async fn test_coordinated_mode_acquires_lock_on_subscribe() {
    // Setup with InstanceMode::Coordinated
    // Subscribe a projection
    // Verify the advisory lock was acquired for that subscriber_id
}

#[tokio::test]
async fn test_coordinated_mode_skips_subscribe_if_lock_held() {
    // Setup two event bus instances with InstanceMode::Coordinated
    // First instance subscribes - should acquire lock
    // Second instance subscribes same subscriber_id - should skip
    // Verify only first instance processes events
}

#[tokio::test]
async fn test_coordinated_mode_allows_different_subscribers() {
    // Setup with InstanceMode::Coordinated
    // Subscribe projection A
    // Subscribe projection B (different subscriber_id)
    // Both should acquire their locks and process events
}
```

## 6. Backward Compatibility

- Adding a new variant to `#[non_exhaustive]` enum is non-breaking
- `SingleInstance` remains the default, so existing code works unchanged
- Users must explicitly opt-in to `Coordinated` mode

## 7. Success Criteria

- [x] `InstanceMode::Coordinated` variant exists
- [x] `subscribe()` acquires advisory lock when in coordinated mode
- [x] `subscribe()` gracefully skips when lock is already held
- [x] Different subscribers can coexist on the same instance
- [x] All existing tests pass
- [x] No clippy warnings
- [x] Code is formatted

## 8. Future Considerations

- **Lock health monitoring**: Periodic check that lock is still held
- **Explicit lock release on unsubscribe**: Add `unsubscribe()` method that releases the lock
- **Lock acquisition retry**: Periodically retry lock acquisition for standby instances
