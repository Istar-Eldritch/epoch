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

### Logical Hierarchy: Aggregate and AggregateRepository

From a logical perspective, the `AggregateRepository` acts as the primary interface for interacting with an `Aggregate`. It is responsible for orchestrating the `Aggregate`'s lifecycle, including:
*   **Loading**: Fetching events (and snapshots) from the `EventStore` and rehydrating the `Aggregate`'s state by applying those events via its `apply` method.
*   **Command Handling**: Providing the `Aggregate` instance to process commands, which in turn produce new events.
*   **Saving**: Persisting the newly generated events to the `EventStore` and managing snapshot creation.

Therefore, while the `Aggregate` encapsulates the business logic and state transitions, a dedicated `AggregateRepository` provides the necessary infrastructure for the `Aggregate` to operate within the system.

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

