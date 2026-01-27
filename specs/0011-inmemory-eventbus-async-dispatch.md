# Specification: InMemoryEventBus Async Dispatch via Channels

**Status:** ✅ Complete  
**Created:** 2026-01-27  
**Completed:** 2026-01-27

## Problem Statement

`InMemoryEventBus` used synchronous sequential dispatch, creating a blocking call chain that caused race conditions in sagas and diverged from production behavior:

1. Aggregate handles command → stores event → publishes to bus
2. Bus **blocks** and immediately calls observer.on_event()
3. Saga handles event → dispatches command → stores event → publishes
4. Saga receives new event **before its state from step 3 is persisted**

In contrast, `PgEventBus`:
- `publish()` is effectively a NO-OP that returns immediately
- Events delivered via NOTIFY/LISTEN asynchronously
- Background listener task processes events from queue
- No nested call chains

This meant tests using `InMemoryEventBus` required manual `tokio::spawn` workarounds that weren't needed in production, making tests diverge from production code patterns.

## Solution Overview

Changed `InMemoryEventBus` to use channel-based async dispatch matching `PgEventBus` architecture:

- `publish()` sends event to unbounded channel and returns immediately
- Background task receives events from channel
- Background task dispatches to observers sequentially (maintains order)
- Breaks synchronous call chain

## Key Design Decisions

### Channel-Based Architecture

```rust
pub struct InMemoryEventBus<D> {
    event_tx: tokio::sync::mpsc::UnboundedSender<Arc<Event<D>>>,
    _listener_handle: Arc<tokio::task::JoinHandle<()>>,
    // ...
}
```

| Component | Purpose |
|-----------|---------|
| Unbounded channel | Matches PgEventBus NO-OP + NOTIFY semantics (never blocks) |
| Background task | Mimics PgEventBus LISTEN loop |
| Sequential processing | Maintains event ordering per observer |

### Unbounded vs Bounded Channel

**Decision:** Use `UnboundedSender`

**Rationale:**
- Matches PgEventBus behavior (NOTIFY doesn't block)
- `publish()` never blocks
- Simple - no backpressure complexity
- In-memory use is testing/development; unbounded queue not a concern

### Sequential Observer Processing

Background task processes observers one at a time:
- Maintains event ordering guarantees
- Simpler error handling
- Matches PgEventBus (sequential per subscriber)

**Not** spawning concurrent tasks per observer - only breaking the synchronous call chain.

### Background Task Lifecycle

Task runs indefinitely. When `InMemoryEventBus` drops:
1. Sender (`event_tx`) dropped
2. Receiver gets `None` from `recv()`
3. Task exits cleanly

No explicit shutdown needed.

## Final API

### Publishing (No External Changes)

```rust
// EventBus trait unchanged - implementation now non-blocking
fn publish(&self, event: Arc<Event<D>>) -> Pin<Box<dyn Future<...>>> {
    Box::pin(async move {
        self.event_tx.send(event)?;  // Returns immediately
        Ok(())
    })
}
```

### Construction

```rust
let bus = InMemoryEventBus::new();  // Same API, now spawns background task
```

## Impact Summary

| Area | Impact |
|------|--------|
| **Backward Compatibility** | ✅ Full - no API changes |
| **Test Code** | Manual `tokio::spawn` workarounds no longer needed |
| **Saga Pattern** | Can dispatch commands directly (no spawn) |
| **Performance** | Slight latency increase (~channel hop), throughput likely improved |
| **Memory** | Minimal (channel buffer for in-flight events) |

## Alternatives Considered

### 1. Keep Synchronous + Fix Saga Pattern

Require sagas to always manually spawn commands to avoid race conditions.

**Rejected:** Makes testing diverge from production behavior. Workarounds shouldn't be necessary.

### 2. Mode Flag for Sync/Async

```rust
InMemoryEventBus::new(EventBusMode::Asynchronous)
```

**Rejected:** Adds complexity. Async behavior is universally better for matching production.

### 3. Concurrent Dispatch (Spawn per Observer)

**Rejected:** Doesn't match PgEventBus sequential processing model. Adds complexity without clear benefit.

## Breaking Changes

None - fully backward compatible.

## Future Enhancements

- **Dead Letter Queue:** Like PgEventBus, could track failed observer invocations
- **Graceful Shutdown:** Explicit method to drain channel and stop background task
- **Metrics:** Track channel depth, processing latency

---

**Key Principle:** In-memory implementations should behave like production backends to avoid test-only patterns.
