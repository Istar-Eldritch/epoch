# Specification: InstanceMode::Coordinated

**Spec ID:** 0004  
**Status:** ✅ Implemented  
**Created:** 2026-01-26  
**Completed:** 2026-01-26  

## Problem Statement

The `InstanceMode` enum only had `SingleInstance` variant. Multi-instance deployments required manual advisory lock management, which was error-prone and didn't integrate with subscribe/listener lifecycle.

## Solution Overview

Added `InstanceMode::Coordinated` that automatically handles PostgreSQL advisory locks during `subscribe()` and listener reconnection.

### Behavior

When `InstanceMode::Coordinated`:

1. **During subscribe():** Attempt to acquire advisory lock for subscriber_id
   - If acquired: Proceed with catch-up and register for events
   - If not acquired: Log info and return Ok() (another instance processing)

2. **During reconnection:** Re-attempt lock acquisition after NOTIFY reconnect
   - If acquired: Resume processing
   - If not acquired: Skip (another instance took over)

3. **On crash/disconnect:** PostgreSQL auto-releases lock (session-scoped)

## Key Design Decisions

### Advisory Lock Implementation

Uses MD5 hash split into two int4 values for 64-bit key space:
```sql
SELECT pg_try_advisory_lock(
  ('x' || substr(md5($1), 1, 8))::bit(32)::int,
  ('x' || substr(md5($1), 9, 8))::bit(32)::int
)
```

**Why dual-int4 instead of hashtext?**
- `hashtext()` returns 32-bit int (~1% collision at 10k subscribers)
- MD5 approach provides 64-bit space (negligible collision risk at 100k+ subscribers)
- Hash collisions cause silent failures (one subscriber stops entirely)

### Failover Behavior

- Advisory locks tied to database session
- Crash/disconnect auto-releases lock
- Another instance acquires on next attempt
- Natural failover without coordination

### Scaling Pattern

For horizontal scaling, run **different** projections on different instances:
```
Instance A: projection:users, projection:orders
Instance B: projection:inventory, projection:reports  
Instance C: saga:fulfillment, saga:notifications
```

For true horizontal scaling of single subscriber across instances, see spec 0002 (Future: Horizontal Scaling with outbox pattern).

## API

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum InstanceMode {
    /// No coordination. Use when single instance or external orchestration.
    #[default]
    SingleInstance,
    
    /// Use advisory locks for coordination. Provides automatic failover.
    Coordinated,
}
```

## Lock Connection Management

Advisory locks are session-scoped. Lock acquired during `subscribe()` on a dedicated connection from pool.

**Simple approach used:**
- Acquire lock at subscribe time
- Don't explicitly track connection
- Lock released when:
  - Application shuts down
  - Connection closed by pool (idle timeout)
  - Database session ends

Sufficient for most use cases. More sophisticated tracking could be added later.

## Backward Compatibility

- Adding variant to `#[non_exhaustive]` enum is non-breaking
- `SingleInstance` remains default
- Users opt-in to `Coordinated` mode

## Future Considerations

- **Lock health monitoring:** Periodic check lock still held
- **Explicit unsubscribe():** Method to release lock
- **Lock acquisition retry:** Standby instances periodically retry
