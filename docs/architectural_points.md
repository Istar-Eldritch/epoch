# Design Rationale and Considerations for Epoch

This document outlines the key architectural decisions, design patterns, and underlying rationale behind the `epoch` project. It aims to provide a comprehensive understanding of the system's structure, the principles guiding its development, and the considerations that shaped its current form. This includes discussions on core event-sourcing concepts, aggregate and projection management, and snapshotting strategies.


### N-M Relationship Between Events and Streams

An N-M (many-to-many) relationship between events and streams, where one event could belong to multiple streams, was considered.

This model is inconsistent with the principles of event sourcing. It breaks the concept of an aggregate as a consistency boundary, invalidates stream-based versioning for concurrency control, and complicates state rehydration.

The conventional one-to-many (1-N) relationship (one event belongs to exactly one stream) should be maintained. Communication between different streams should be handled via an event bus and reactive projections/processes.


### It was considered removing EventStreams and using Projections for aggregation operations.

The `EventStream` and `Projection` traits serve distinct and essential purposes:

*   **`EventStream`**: Provides the ability to **read and replay the historical sequence of events** from the store. This is fundamental for rehydrating aggregates and rebuilding projections.
*   **`Projection`**: Consumes events (both historical and live) to **build and update a read model**.

The `Projection` does not replace the `EventStream`. Removing the `EventStream` would eliminate the ability to replay history, a fundamental aspect of event sourcing.

## It was considered removing the Aggregate abstraction and use Projections uniquely as a way to keep traditional ES aggregates.

Aggregates and projections serve logically different purposes:

*   **Aggregates (Write Model)**: Aggregates are the core of the write model. They are responsible for encapsulating business logic, enforcing invariants, processing commands, and emitting events based on those commands. They represent the current state of an entity and are the source of truth for all changes.

*   **Projections (Read Model)**: Projections are read models. Their sole purpose is to consume events and transform them into a queryable, often denormalized, view of the data. They do not handle commands, enforce business rules, or emit new events; they are passive consumers of events. While the `apply` method of a projection should ideally be free of side effects that impact the write model or emit new domain events, the act of *persisting* the read model itself (e.g., writing to a database) is an intended and necessary side effect. This persistence is typically handled by a separate component or by the projection infrastructure, ensuring the `apply` method remains focused on pure data transformation.

Given these distinct responsibilities, aggregates should not be managed by projections. Combining these roles could result in an architecture that deviates from event sourcing principles, potentially impacting consistency, debugging, and independent scaling. Therefore, a new abstraction for aggregates is necessary within `epoch_core` to manage their lifecycle, command handling, and event emission, separate from the `Projection` trait.

This architecture supports separation of concerns, contributing to the `epoch` project's maintainability, testability, and adaptability for various event-sourcing use cases.

## Relationship Between Aggregate and Projection

In `epoch_core`, an `Aggregate` is a specialized form of `Projection`, unifying the write model's state management with a read model's event-driven updates.

*   **Aggregate (Write Model and Self-Projecting Entity):**
    *   Receives and processes `Command`s, encapsulating business logic and enforcing invariants.
    *   Emits `Event`s as a result of state transitions.
    *   Critically, the `Aggregate` trait is defined as `pub trait Aggregate<ED>: Projection<ED>`, meaning any type implementing `Aggregate` must also implement `Projection`.
    *   It leverages its `Projection` capabilities (specifically the `re_hydrate` method) to update its *own* internal state from the events it has just generated or retrieved from the `EventStore`. This ensures the aggregate's state is always consistent with its own event stream.
    *   Responsible for persisting both the emitted events to the `EventStoreBackend` and its current state to the `StateStoreBackend`.

*   **Projection (General Read Model):**
    *   A broader concept defining how to build and maintain a read-optimized model from any stream of `Event`s.
    *   Provides methods like `apply_create`, `apply_update`, and `apply_delete` to transform events into a `ProjectionState`.
    *   While `Aggregates` use `Projection` for their internal state, other independent `Projections` can also exist within the system to create various denormalized views of data for querying purposes (e.g., dashboards, search indexes), without handling commands or emitting events themselves.

This design choice ensures that the `Aggregate` efficiently manages its state consistency by directly applying its own events, while the `Projection` trait provides a flexible foundation for all event-driven state reconstruction within the system.

### Logical Hierarchy: Aggregate and AggregateRepository

From a conceptual perspective, the `AggregateRepository` acts as a primary interface for interacting with an `Aggregate`. This conceptual role encompasses orchestrating the `Aggregate`'s lifecycle, including:
*   **Loading**: Fetching events (and snapshots) from the `EventStore` and rehydrating the `Aggregate`'s state by applying those events via its `apply` method.
*   **Command Handling**: Providing the `Aggregate` instance to process commands, which in turn produce new events.
*   **Saving**: Persisting the newly generated events to the `EventStore` and managing snapshot creation.

Crucially, in this codebase, the responsibilities traditionally associated with a dedicated `AggregateRepository` are distributed and fulfilled by the collaborative interaction of the `Aggregate` trait itself (which orchestrates command handling and interactions with storage) and the generic `EventStoreBackend` and `StateStoreBackend` traits (which provide concrete persistence mechanisms). This design pattern ensures that the `Aggregate` remains focused purely on domain logic, relying on pluggable infrastructure components for its operational needs.

### Design Rationale: Separation of `Aggregate` and `AggregateRepository`

A key architectural decision is the separation of the `Aggregate` from the `AggregateRepository`. An alternative approach, merging them to co-locate all related logic (a principle known as Locality of Behavior), was considered. This would place the `load` and `save` methods directly on the `Aggregate` struct.

While this approach appears simpler at first glance, it was not adopted due to the following considerations, which emphasize the application of **Separation of Concerns (SoC)** and the preservation of a **Domain Model's integrity** over Locality of Behavior in this context.

*   **Distinct Roles and Dependency Direction**:
    *   **`Aggregate`**: Its fundamental role is to encapsulate business logic and enforce invariants. It represents a consistency boundary within the domain. It should be "pure," meaning it has no knowledge or dependency on how it is persisted.
    *   **`AggregateRepository`**: Its role is to mediate between the domain layer and the persistence layer. It provides a collection-like interface for loading and saving aggregates, encapsulating the orchestration of persistence mechanisms (like `EventStoreBackend` and `SnapshotBackend`).
    This separation ensures that the core `Aggregate` remains free from infrastructure concerns, and dependencies flow correctly: infrastructure (Repository) depends on the domain (Aggregate trait), but the domain does not depend on infrastructure.

*   **Maintaining Purity of the Domain Model**:
    Even with Rust's blanket implementations for traits, if `load` and `save` methods were part of the `Aggregate` trait, the trait definition itself would necessarily introduce dependencies on persistence concepts (`EventStoreBackend`, `SnapshotBackend`). This pollutes the domain model's interface with infrastructure details, making it less pure and harder to reason about in isolation from persistence.

*   **Clear Separation of Concerns (SoC)**:
    The `Aggregate` and `Repository` have different reasons to change.
    *   **`Aggregate`**: Changes when **business requirements** change.
    *   **`AggregateRepository`**: Changes when **technical or infrastructure requirements** change (e.g., migrating databases, implementing a new caching strategy).

    Separating them ensures that the core domain logic remains pure and decoupled from infrastructure concerns. This contributes to system stability, as technical changes are less likely to impact business logic.

While Locality of Behavior is a recognized principle, its effectiveness is maximized when co-located code shares a common reason for change. In this context, utilizing a single, reusable `AggregateRepository` for generic persistence concerns across aggregates offers advantages that are considered more significant than co-locating that logic with each aggregate's specific business rules.

## Snapshotting Strategy

Can a `Projection` be used to create and manage snapshots for aggregates?

A snapshot is an optimization for the **write model**, designed to reduce the time it takes to rehydrate an aggregate from its event stream. Its lifecycle and management are intrinsically tied to the aggregate's persistence and loading mechanism (e.g., an Aggregate Repository). In contrast, a `Projection` is part of the **read model**, responsible for creating queryable views for external clients. Requiring a projection to manage snapshots would conflict with the established separation of concerns (CQRS) within the `epoch` architecture.

Snapshotting logic should be handled by the write model's persistence layer. The appropriate implementation is to introduce an `AggregateRepository` that encapsulates the logic for loading an aggregate. This repository will be responsible for fetching the latest snapshot, replaying subsequent events, and deciding when to create a new snapshot after an aggregate's state has been updated. This maintains a clear separation between write-side and read-side concerns.

## EventBus and EventStoreBackend Interaction

This section discusses the interaction between the `EventBus` and `EventStoreBackend`, particularly concerning event persistence and publication.

### `EventBus::publish` as a No-Op with PostgreSQL Notifications

**Proposal:** To implement `EventBus` for PostgreSQL, the `publish` method would be a no-op. Event persistence would be handled by the `EventStoreBackend` for PostgreSQL, which would then trigger a PostgreSQL notification (`NOTIFY`) to inform subscribers.

**Rationale and Outcomes:**

*   **Transactional Consistency:** This design ensures strong transactional consistency. Events are only "published" (via PostgreSQL `NOTIFY`) after they have been successfully and durably persisted to the event store by the `EventStoreBackend`. This guarantees that only committed events are propagated, which is fundamental for data integrity in event-sourced systems.
*   **Alignment with Event Sourcing:** The approach reinforces the event store as the single source of truth. Event persistence naturally triggers downstream processing, maintaining a clear flow where the persistence layer is responsible for the initial event emission, and the `EventBus` for event reception and distribution.
*   **Separation of Concerns (Nuanced):** While `publish` becomes a no-op for the `EventBus`, it still serves its role for subscription and delivery of *persisted* events. The `EventStoreBackend` explicitly handles the persistence. This maintains a clear, albeit nuanced, separation where the `EventBus` is focused on event distribution, and the `EventStoreBackend` on durable storage.
*   **Simplified `EventBus` Implementation:** The PostgreSQL `EventBus` implementation becomes simpler by offloading the actual event writing to the `EventStoreBackend`.
*   **Documentation:** Clear documentation is crucial to explain that the `EventBus::publish` method for the PostgreSQL implementation is a no-op, and that event propagation is implicitly handled by the `EventStoreBackend`'s persistence mechanism via database notifications.
*   **Delivery Guarantees:** PostgreSQL `NOTIFY` provides "at-most-once" delivery for immediate notifications. For robust, "at-least-once" processing and eventual consistency, projections should primarily rely on reading events from the `EventStream` provided by the `EventStoreBackend`. The `EventBus` in this context serves as a low-latency mechanism for notifying subscribers of *newly persisted events*.
*   **Scalability:** For extremely high-throughput scenarios, `NOTIFY` might become a bottleneck. However, for many typical event-sourcing applications, it's sufficient and offers a simpler operational footprint than dedicated message queues.

### Inverting Store and Bus Responsibilities

**Proposal:** An alternative where the `EventStoreBackend` uses the `EventBus` to persist events (i.e., `PgEventBus` would both store and read events, and `PgEventStore` would use `PgEventBus` for persistence) was considered.

**Rationale and Outcomes (Reasons for Rejection):**

*   **Violation of Separation of Concerns (SoC):** This inversion merges the distinct responsibilities of persistence (`EventStoreBackend`) and event distribution (`EventBus`) into a single component (`PgEventBus`). This goes against the `epoch` project's design principle of SoC, leading to a less maintainable and harder-to-understand codebase.
*   **Increased Coupling and Inverted Dependencies:** The core persistence component (`EventStoreBackend`) would become dependent on the event distribution component (`EventBus`), creating an inverted and undesirable dependency flow. This increases coupling and reduces system flexibility.
*   **Compromised Transactional Guarantees:** If the `EventBus` manages persistence, the transactional boundary becomes less clear. The `EventBus` would need to manage database transactions, a responsibility typically belonging to the persistence layer. This could complicate error handling and make it harder to guarantee that events are both persisted and reliably delivered. The current model (persistence first, then notification) provides stronger guarantees.
*   **Deviation from Event Sourcing Principles:** Event Sourcing establishes the Event Store as the central, immutable source of truth. Delegating the core persistence responsibility of the `EventStoreBackend` to the `EventBus` diminishes the `EventStoreBackend`'s role and makes the `EventBus` the de-facto event store for writes, which deviates from conventional event-sourced system architecture.
*   **Increased Complexity:** This inversion adds an extra layer of indirection to the write path, making the system harder to reason about and debug.

