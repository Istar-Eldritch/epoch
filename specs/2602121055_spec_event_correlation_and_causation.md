# Specification: Event Correlation and Causation Tracking

**Document ID:** 2602121055  
**Status:** ✅ Implemented  
**Created:** 2026-02-12  
**Completed:** 2026-02-14  
**Author:** AI Agent  

---

## Problem Statement

When a user places an order and a saga coordinates inventory reservation, payment processing, and order confirmation across three aggregates, there is no way to query "show me everything that happened because of this order" or "what specifically caused this payment event." Events are isolated in their individual streams with no metadata linking them.

Epoch's `Event` struct had no concept of *why* an event was produced or *what other events* it's related to. The `Command` struct carried no context about what triggered it. The `EventStoreBackend` trait only supported querying by `stream_id` — there was no cross-stream query capability.

This made debugging, auditing, and process tracing impossible at the framework level.

## Solution Overview

Added `correlation_id` and `causation_id` metadata to both `Command` and `Event`, with semi-automatic propagation through `Aggregate::handle()`, ergonomic helpers for sagas, and query APIs on `EventStoreBackend`.

### Causation Semantics

- **`causation_id: Option<Uuid>`** — Points to the specific event that directly caused this event. For user-triggered commands, this is `None`. When a saga reacts to event A and dispatches a command that produces event B, then `B.causation_id = Some(A.id)`. Forms a parent→child chain.

- **`correlation_id: Option<Uuid>`** — A shared group identifier tying together the entire causal tree rooted at the original user action. Auto-generated from the first event's `id` if not explicitly provided. All downstream events inherit the same `correlation_id`.

### Propagation Model

- **Inside `Aggregate::handle()`**: Fully automatic. The framework stamps `correlation_id` and `causation_id` from the command onto all events produced by `handle_command()`. If no `correlation_id` is on the command, one is auto-generated from the first event's `id`.

- **At saga boundaries**: Explicit via `Command::new(...).caused_by(&event)` helper. This keeps the `Saga::handle_event` signature unchanged and avoids hidden global state.

### Query Model

Two new methods on `EventStoreBackend`:

- **`read_events_by_correlation_id(correlation_id)`** — Returns all events sharing a correlation ID, ordered by `global_sequence`. Answers "show me everything that happened because of this user action."

- **`trace_causation_chain(event_id)`** — Returns the ancestors, the event itself, and all descendants in the causal subtree — *excluding* unrelated branches that merely share the same `correlation_id`. Answers "what specifically led to and followed from this event."

Both return `Vec<Event<Self::EventType>>` (not streams), since correlation groups are bounded and small (typically 5-50 events).

## Key Design Decisions

### Semi-Automatic vs Fully Automatic Propagation

**Decision:** Automatic inside aggregates, explicit at saga boundaries.

**Rationale:**
- Aggregates produce events atomically — framework control is safe and ergonomic
- Sagas dispatch commands asynchronously across boundaries — explicit threading prevents bugs and maintains clarity
- The `.caused_by(&event)` helper is one line and makes causation visible in saga code

**Alternatives Considered:**
- Thread-local storage to auto-propagate: Hidden magic, breaks with async executors
- Change `Saga::handle_event` signature to include context: Breaking change for all sagas

### Correlation ID Auto-Generation

**Decision:** Auto-generate from the first event's ID if the command has none.

**Rationale:**
- Entry points may not know they should set a correlation ID
- All events in a workflow should share one ID — using the first event's ID is deterministic and requires no coordination
- Still allows explicit injection via `.with_correlation_id()` for distributed tracing integration

### Query Implementation Strategy

**Decision:** `trace_causation_chain` fetches the full correlation group once, then filters in Rust.

**Rationale:**
- Causation trees are small (5-50 events typically)
- One SQL query + in-memory filtering is faster than multiple round-trips to walk the tree recursively
- Easier to test and reason about than complex recursive CTEs

**Alternatives Considered:**
- PostgreSQL recursive CTE: More complex, harder to port to other backends
- Multiple queries walking the tree: N+1 query problem

### Causation Tree Utility

**Decision:** Extracted `extract_causation_subtree()` as a pure function in `epoch_core/src/causation.rs`.

**Rationale:**
- Logic is identical across all backends (Pg, InMemory, future implementations)
- Pure function is easier to unit test
- Keeps backend implementations focused on data fetching

## API Changes

### Event Struct

```rust
pub struct Event<D: EventData> {
    // ... existing fields ...

    /// The ID of the event that directly caused this event to be produced.
    ///
    /// `None` for events triggered by direct user commands (no prior event in the chain).
    /// `Some(event_id)` when this event was produced as a consequence of another event
    /// (e.g., via a saga reacting to an event and dispatching a command).
    pub causation_id: Option<Uuid>,

    /// A shared identifier tying together all events in a causal tree.
    ///
    /// All events originating from the same user action share the same `correlation_id`.
    /// Auto-generated by `Aggregate::handle()` if not provided on the command.
    /// `None` for events that predate causation tracking.
    pub correlation_id: Option<Uuid>,
}
```

### Command Struct

```rust
pub struct Command<D, C> {
    // ... existing fields ...

    pub causation_id: Option<Uuid>,
    pub correlation_id: Option<Uuid>,
}

impl<D, C> Command<D, C> {
    /// Sets causation context from a triggering event.
    ///
    /// Use this in saga `handle_event` implementations to thread causal context.
    pub fn caused_by<ED: EventData>(self, event: &Event<ED>) -> Self;

    /// Explicitly sets a correlation ID.
    ///
    /// Use this at entry points (e.g., HTTP handlers) to inject an external
    /// trace ID as the correlation ID for all downstream events.
    pub fn with_correlation_id(self, correlation_id: Uuid) -> Self;
}
```

### EventStoreBackend Trait

```rust
/// Returns all events sharing the given correlation ID, ordered by global_sequence.
async fn read_events_by_correlation_id(
    &self,
    correlation_id: Uuid,
) -> Result<Vec<Event<Self::EventType>>, Self::Error>;

/// Returns the causal subtree for the given event: its ancestors, the event
/// itself, and all descendants — excluding unrelated branches that share
/// the same correlation ID but are not in the direct causal path.
async fn trace_causation_chain(
    &self,
    event_id: Uuid,
) -> Result<Vec<Event<Self::EventType>>, Self::Error>;
```

## Usage Example

```rust
// Entry point: inject correlation ID for distributed tracing
let trace_id = Uuid::new_v4();
let cmd = Command::new(order_id, PlaceOrder { items }, None, None)
    .with_correlation_id(trace_id);
order_aggregate.handle(cmd).await?;

// Saga: thread causation context explicitly
async fn handle_event(&self, state: Self::State, event: &Event<Self::EventType>) 
    -> Result<Option<Self::State>, Self::SagaError> 
{
    match event.data.as_ref().unwrap() {
        OrderEvent::OrderPlaced { .. } => {
            let cmd = Command::new(shipping_id, ShipOrder { order_id }, None, None)
                .caused_by(event);  // ← Threads causation + correlation
            
            self.shipping_aggregate.handle(cmd).await?;
            Ok(Some(SagaState::Shipped))
        }
    }
}

// Query all events in the workflow
let all_events = event_store.read_events_by_correlation_id(trace_id).await?;

// Query causal path through a specific event
let causal_chain = event_store.trace_causation_chain(shipped_event_id).await?;
```

**Example scenario:**
```
PlaceOrder command (correlation_id = trace_id)
  └─ OrderPlaced event
       ├─ NotifyPlacement → PlacementNotificationSent
       └─ ShipOrder → OrderShipped
            ├─ ConfirmOrder → OrderConfirmed
            └─ NotifyShipment → ShipmentNotificationSent
```

Full example: `epoch/examples/event-correlation-causation.rs`

## Database Schema

PostgreSQL migration `m007_add_causation_columns` added:
- `correlation_id UUID` (nullable)
- `causation_id UUID` (nullable)
- Index on `correlation_id` for query performance
- Updated NOTIFY trigger to include new fields

In-memory store maintains secondary index `HashMap<Uuid, Vec<Uuid>>` for correlation lookups.

## Backward Compatibility

- `Command::new()` signature unchanged — new fields default to `None`
- `Event` fields are `Option<Uuid>`, no migration required
- Existing events have `correlation_id = NULL`, meaning "predates tracking"
- New trait methods are required but all implementations are in-tree

## Future Enhancements

- **Causation visualization tools** — Generate graphs from causation chains
- **Correlation metadata** — Allow attaching key-value pairs to correlation IDs for tagging entire workflows
- **Cross-system correlation** — Standardize on W3C Trace Context format for distributed tracing integration
- **Performance monitoring** — Detect anomalously large correlation groups (potential infinite loops)
