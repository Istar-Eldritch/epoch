# Design Rationale and Considerations for Epoch

This document outlines the key architectural decisions, design patterns, and underlying rationale behind the `epoch` project. It aims to provide a comprehensive understanding of the system's structure, the principles guiding its development, and the considerations that shaped its current form.

## Core Design Philosophy

Epoch takes an **opinionated approach to event sourcing** that prioritizes simplicity and performance over traditional patterns. The key insight is:

> **Aggregates are living snapshots.** Every time an event affects an aggregate, its state is computed and immediately persisted. This eliminates the need for separate snapshotting strategies entirely.

This design trades slightly more write overhead for:
- **Fast reads** - No event replay needed; just fetch the current state
- **Simpler architecture** - No snapshot intervals, thresholds, or management logic
- **Always consistent** - State is updated atomically with event storage
- **Predictable performance** - Read performance doesn't degrade as event streams grow

---

## Event and Stream Relationships

### N-M Relationship Between Events and Streams

An N-M (many-to-many) relationship between events and streams, where one event could belong to multiple streams, was considered.

This model is inconsistent with the principles of event sourcing. It breaks the concept of an aggregate as a consistency boundary, invalidates stream-based versioning for concurrency control, and complicates state rehydration.

The conventional one-to-many (1-N) relationship (one event belongs to exactly one stream) should be maintained. Communication between different streams should be handled via an event bus and reactive projections/processes.

### EventStream vs Projection

It was considered removing EventStreams and using Projections for aggregation operations.

The `EventStream` and `Projection` traits serve distinct and essential purposes:

*   **`EventStream`**: Provides the ability to **read and replay the historical sequence of events** from the store. This is fundamental for rebuilding projections and disaster recovery.
*   **`Projection`**: Consumes events (both historical and live) to **build and update a read model**.

The `Projection` does not replace the `EventStream`. Removing the `EventStream` would eliminate the ability to replay history, a fundamental aspect of event sourcing.

---

## Aggregates and Projections

### Why Both Aggregates and Projections?

It was considered removing the Aggregate abstraction and using Projections uniquely as a way to keep traditional ES aggregates.

Aggregates and projections serve logically different purposes:

*   **Aggregates (Write Model)**: Aggregates are the core of the write model. They are responsible for encapsulating business logic, enforcing invariants, processing commands, and emitting events based on those commands. They represent the current state of an entity and are the source of truth for all changes.

*   **Projections (Read Model)**: Projections are read models. Their sole purpose is to consume events and transform them into a queryable, often denormalized, view of the data. They do not handle commands, enforce business rules, or emit new events; they are passive consumers of events. While the `apply` method of a projection should ideally be free of side effects that impact the write model or emit new domain events, the act of *persisting* the read model itself (e.g., writing to a database) is an intended and necessary side effect.

Given these distinct responsibilities, aggregates should not be managed by projections. Combining these roles could result in an architecture that deviates from event sourcing principles, potentially impacting consistency, debugging, and independent scaling.

### Relationship Between Aggregate and Projection

In `epoch_core`, an `Aggregate` is a specialized form of `Projection`, unifying the write model's state management with a read model's event-driven updates.

*   **Aggregate (Write Model and Self-Projecting Entity):**
    *   Receives and processes `Command`s, encapsulating business logic and enforcing invariants.
    *   Emits `Event`s as a result of state transitions.
    *   The `Aggregate` trait is defined as `pub trait Aggregate<ED>: Projection<ED>`, meaning any type implementing `Aggregate` must also implement `Projection`.
    *   It leverages its `Projection` capabilities (specifically the `apply` and `re_hydrate` methods) to update its *own* internal state from the events it has just generated.
    *   **Crucially, the aggregate state is persisted immediately after each command**, acting as a living snapshot. This is the key architectural decision that eliminates traditional snapshotting.

*   **Projection (General Read Model):**
    *   A broader concept defining how to build and maintain a read-optimized model from any stream of `Event`s.
    *   Provides the `apply` method to transform events into a `ProjectionState`.
    *   Independent `Projections` can exist within the system to create various denormalized views of data for querying purposes (e.g., dashboards, search indexes), without handling commands or emitting events themselves.

This design choice ensures that the `Aggregate` efficiently manages its state consistency by directly applying its own events, while the `Projection` trait provides a flexible foundation for all event-driven state reconstruction within the system.

---

## State Persistence Strategy (No Traditional Snapshotting)

### The "Living Snapshot" Approach

Traditional event sourcing requires snapshotting as an optimization—periodically saving aggregate state to avoid replaying potentially thousands of events on every load. Epoch takes a different approach:

**Every aggregate state change is immediately persisted.**

When a command is processed:
1. Events are generated by the aggregate's business logic
2. Events are stored in the `EventStoreBackend`
3. The aggregate's state is recomputed by applying the new events
4. **The new state is immediately persisted to the `StateStoreBackend`**

This means:
- Loading an aggregate is a single state fetch (O(1)), not an event replay (O(n))
- No need to decide snapshot intervals or thresholds
- No separate snapshot storage or management
- State is always consistent with the event stream

### Trade-offs

| Aspect | Traditional Snapshotting | Epoch's Living Snapshots |
|--------|-------------------------|--------------------------|
| Read performance | Degrades over time without snapshots | Constant O(1) |
| Write performance | Events only | Events + state (slightly more) |
| Complexity | Snapshot scheduling, storage, cleanup | None |
| Storage | Events + periodic snapshots | Events + current state |
| Recovery | Replay from last snapshot | Just fetch current state |

### Why Not a Separate AggregateRepository?

Traditional event sourcing often uses an `AggregateRepository` to orchestrate:
- Loading aggregates (fetching snapshots + replaying events)
- Saving aggregates (persisting events + managing snapshots)

In Epoch, these responsibilities are handled directly by the `Aggregate` trait's `handle` method, which coordinates with the `EventStoreBackend` and `StateStoreBackend`. This is intentional:

- The "living snapshot" approach eliminates the need for complex loading logic
- State persistence is atomic with event storage
- The `Aggregate` trait provides a complete, self-contained abstraction

---

## EventBus and EventStoreBackend Interaction

### `EventBus::publish` as a No-Op with PostgreSQL Notifications

To implement `EventBus` for PostgreSQL, the `publish` method is a no-op. Event persistence is handled by the `EventStoreBackend` for PostgreSQL, which triggers a PostgreSQL notification (`NOTIFY`) to inform subscribers.

**Rationale:**

*   **Transactional Consistency:** Events are only "published" (via PostgreSQL `NOTIFY`) after they have been successfully and durably persisted. This guarantees that only committed events are propagated.
*   **Alignment with Event Sourcing:** The event store remains the single source of truth. Event persistence naturally triggers downstream processing.
*   **Simplified Implementation:** The PostgreSQL `EventBus` implementation is simpler by offloading event writing to the `EventStoreBackend`.

**Important Considerations:**

*   **Delivery Guarantees:** PostgreSQL `NOTIFY` provides "at-most-once" delivery. For robust processing, projections should be able to rebuild from the `EventStream` if needed. The `EventBus` serves as a low-latency notification mechanism for *newly persisted events*.
*   **Scalability:** For extremely high-throughput scenarios, `NOTIFY` might become a bottleneck. For many typical applications, it's sufficient and offers simpler operations than dedicated message queues.

### Why Not Invert Store and Bus Responsibilities?

An alternative where the `EventStoreBackend` uses the `EventBus` to persist events was considered and rejected:

*   **Violation of Separation of Concerns:** This merges persistence and distribution responsibilities into one component.
*   **Compromised Transactional Guarantees:** The transactional boundary becomes unclear when the `EventBus` manages persistence.
*   **Deviation from Event Sourcing Principles:** The Event Store should be the central, immutable source of truth.

---

## Design Decisions Log

This section documents specific design decisions and the alternatives that were considered.

### Decision: Aggregates Extend Projections

**Choice:** `Aggregate<ED>: Projection<ED>`

**Rationale:** Aggregates need to apply events to their own state, which is exactly what projections do. By extending `Projection`, aggregates reuse the `apply` and `re_hydrate` methods without code duplication.

### Decision: Immediate State Persistence

**Choice:** Persist aggregate state after every command, not just events.

**Rationale:** Eliminates snapshotting complexity entirely. The slight write overhead is worth the simplified architecture and consistent read performance.

### Decision: No Separate AggregateRepository

**Choice:** The `Aggregate` trait's `handle` method orchestrates the full command lifecycle.

**Rationale:** With immediate state persistence, there's no complex loading logic to encapsulate. The `Aggregate` trait provides a complete abstraction.

### Decision: PostgreSQL NOTIFY for Event Bus

**Choice:** Use database triggers and `NOTIFY` for event propagation instead of a separate message queue.

**Rationale:** Simplifies deployment and operations. Provides transactional consistency between event persistence and notification. Sufficient for many use cases.
