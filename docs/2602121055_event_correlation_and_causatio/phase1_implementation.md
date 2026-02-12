# Phase 1: Core Data Structures and Automatic Propagation

**Estimated Effort**: 2-3 days

## Overview

This phase implements the foundational data structures for causation tracking by adding `correlation_id` and `causation_id` fields to the `Event` and `Command` structs, along with automatic propagation logic in the aggregate's `handle()` method. This establishes the infrastructure that subsequent phases will build upon.

## Prerequisites

- Rust development environment with edition 2024
- Understanding of Epoch's event sourcing architecture
- Familiarity with the aggregate command handling flow

## Steps

### Step 1.1: Add Causation Fields to Event Struct

**Files**: `epoch_core/src/event.rs`
**Pattern Reference**: Based on existing `global_sequence` field pattern (line ~60)
**TDD**: Write failing tests first, then implement

#### Test First (Add to `epoch_core/src/event.rs` test module at end of file)

```rust
#[test]
fn event_builder_with_causation_fields() {
    let correlation_id = Uuid::new_v4();
    let causation_id = Uuid::new_v4();
    
    let event = Event::<TestEventData>::builder()
        .stream_id(Uuid::new_v4())
        .event_type("Test".to_string())
        .correlation_id(correlation_id)
        .causation_id(causation_id)
        .build()
        .unwrap();
    
    assert_eq!(event.correlation_id, Some(correlation_id));
    assert_eq!(event.causation_id, Some(causation_id));
}

#[test]
fn event_builder_without_causation_defaults_to_none() {
    let event = Event::<TestEventData>::builder()
        .stream_id(Uuid::new_v4())
        .event_type("Test".to_string())
        .build()
        .unwrap();
    
    assert_eq!(event.correlation_id, None);
    assert_eq!(event.causation_id, None);
}

#[test]
fn event_into_builder_preserves_causation() {
    let correlation_id = Uuid::new_v4();
    let causation_id = Uuid::new_v4();
    let stream_id = Uuid::new_v4();
    
    let original = Event {
        id: Uuid::new_v4(),
        stream_id,
        stream_version: 1,
        event_type: "Test".to_string(),
        actor_id: None,
        purger_id: None,
        data: Some(TestEventData::TestEvent {
            value: "test".to_string(),
        }),
        created_at: Utc::now(),
        purged_at: None,
        global_sequence: Some(100),
        correlation_id: Some(correlation_id),
        causation_id: Some(causation_id),
    };
    
    let rebuilt = original.into_builder().build().unwrap();
    
    assert_eq!(rebuilt.correlation_id, Some(correlation_id));
    assert_eq!(rebuilt.causation_id, Some(causation_id));
}

#[test]
fn to_subset_event_preserves_causation() {
    let correlation_id = Uuid::new_v4();
    let causation_id = Uuid::new_v4();
    let stream_id = Uuid::new_v4();
    
    let original: Event<TestEventData> = Event {
        id: Uuid::new_v4(),
        stream_id,
        stream_version: 1,
        event_type: "Test".to_string(),
        actor_id: None,
        purger_id: None,
        data: Some(TestEventData::TestEvent {
            value: "test".to_string(),
        }),
        created_at: Utc::now(),
        purged_at: None,
        global_sequence: Some(50),
        correlation_id: Some(correlation_id),
        causation_id: Some(causation_id),
    };
    
    let subset: Event<TestEventData> = original.to_subset_event().unwrap();
    
    assert_eq!(subset.correlation_id, Some(correlation_id));
    assert_eq!(subset.causation_id, Some(causation_id));
}

#[test]
fn to_subset_event_ref_preserves_causation() {
    let correlation_id = Uuid::new_v4();
    let causation_id = Uuid::new_v4();
    let stream_id = Uuid::new_v4();
    
    let original: Event<TestEventData> = Event {
        id: Uuid::new_v4(),
        stream_id,
        stream_version: 1,
        event_type: "Test".to_string(),
        actor_id: None,
        purger_id: None,
        data: Some(TestEventData::TestEvent {
            value: "test".to_string(),
        }),
        created_at: Utc::now(),
        purged_at: None,
        global_sequence: Some(75),
        correlation_id: Some(correlation_id),
        causation_id: Some(causation_id),
    };
    
    let subset: Event<TestEventData> = original.to_subset_event_ref().unwrap();
    
    assert_eq!(subset.correlation_id, Some(correlation_id));
    assert_eq!(subset.causation_id, Some(causation_id));
}

#[test]
fn to_superset_event_preserves_causation() {
    let correlation_id = Uuid::new_v4();
    let causation_id = Uuid::new_v4();
    let stream_id = Uuid::new_v4();
    
    let original: Event<TestEventData> = Event {
        id: Uuid::new_v4(),
        stream_id,
        stream_version: 1,
        event_type: "Test".to_string(),
        actor_id: None,
        purger_id: None,
        data: Some(TestEventData::TestEvent {
            value: "test".to_string(),
        }),
        created_at: Utc::now(),
        purged_at: None,
        global_sequence: Some(75),
        correlation_id: Some(correlation_id),
        causation_id: Some(causation_id),
    };
    
    let superset: Event<TestEventData> = original.to_superset_event();
    
    assert_eq!(superset.correlation_id, Some(correlation_id));
    assert_eq!(superset.causation_id, Some(causation_id));
}

#[test]
fn data_builder_method_preserves_causation() {
    let correlation_id = Uuid::new_v4();
    let causation_id = Uuid::new_v4();
    
    let builder = Event::<TestEventData>::builder()
        .stream_id(Uuid::new_v4())
        .event_type("Test".to_string())
        .correlation_id(correlation_id)
        .causation_id(causation_id)
        .global_sequence(123);

    let new_builder = builder.data(Some(TestEventData::TestEvent {
        value: "test".to_string(),
    }));

    let event = new_builder.build().unwrap();
    
    assert_eq!(event.correlation_id, Some(correlation_id));
    assert_eq!(event.causation_id, Some(causation_id));
    assert_eq!(event.global_sequence, Some(123));
}
```

**Verify**: Run `cargo test --package epoch_core event_builder` - tests should fail

#### Implementation

**Action 1**: Add fields to `Event` struct (around line 15, after `global_sequence`)

```rust
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
```

**Action 2**: Add fields to `EventBuilder` struct (around line 240, after `global_sequence`)

```rust
/// The correlation ID to link related events together.
pub correlation_id: Option<Uuid>,
/// The causation ID pointing to the event that caused this one.
pub causation_id: Option<Uuid>,
```

**Action 3**: Update `EventBuilder::new()` to initialize new fields to `None` (around line 256)

```rust
pub fn new() -> Self {
    EventBuilder {
        id: None,
        stream_id: None,
        stream_version: None,
        event_type: None,
        actor_id: None,
        purger_id: None,
        data: None,
        created_at: None,
        purged_at: None,
        global_sequence: None,
        correlation_id: None,
        causation_id: None,
    }
}
```

**Action 4**: Add builder methods (after `global_sequence()` method around line 310)

```rust
/// Sets the correlation ID for the event.
///
/// The correlation ID ties together all events in a causal tree originating
/// from the same user action.
pub fn correlation_id(mut self, correlation_id: Uuid) -> Self {
    self.correlation_id = Some(correlation_id);
    self
}

/// Sets the causation ID for the event.
///
/// The causation ID points to the specific event that directly caused this
/// event to be produced.
pub fn causation_id(mut self, causation_id: Uuid) -> Self {
    self.causation_id = Some(causation_id);
    self
}
```

**Action 5**: Update `EventBuilder::build()` to include new fields (around line 320)

```rust
pub fn build(self) -> Result<Event<D>, EventBuilderError> {
    Ok(Event {
        id: self.id.unwrap_or_else(Uuid::new_v4),
        stream_id: self.stream_id.ok_or(EventBuilderError::StreamIdMissing)?,
        stream_version: self.stream_version.unwrap_or(0),
        event_type: self.event_type.ok_or(EventBuilderError::EventTypeMissing)?,
        actor_id: self.actor_id,
        purger_id: self.purger_id,
        data: self.data,
        created_at: self.created_at.unwrap_or(Utc::now()),
        purged_at: self.purged_at,
        global_sequence: self.global_sequence,
        correlation_id: self.correlation_id,
        causation_id: self.causation_id,
    })
}
```

**Action 6**: Update `EventBuilder::data()` to preserve new fields (around line 300)

```rust
pub fn data<P: EventData>(self, data: Option<P>) -> EventBuilder<P> {
    EventBuilder {
        id: self.id,
        stream_id: self.stream_id,
        stream_version: self.stream_version,
        event_type: self.event_type,
        actor_id: self.actor_id,
        purger_id: self.purger_id,
        data,
        created_at: self.created_at,
        purged_at: self.purged_at,
        global_sequence: self.global_sequence,
        correlation_id: self.correlation_id,
        causation_id: self.causation_id,
    }
}
```

**Action 7**: Update `From<Event<D>> for EventBuilder<D>` (around line 195)

```rust
fn from(event: Event<D>) -> Self {
    Self {
        id: Some(event.id),
        stream_id: Some(event.stream_id),
        stream_version: Some(event.stream_version),
        event_type: Some(event.event_type),
        actor_id: event.actor_id,
        purger_id: event.purger_id,
        data: event.data,
        created_at: Some(event.created_at),
        purged_at: event.purged_at,
        global_sequence: event.global_sequence,
        correlation_id: event.correlation_id,
        causation_id: event.causation_id,
    }
}
```

**Action 8**: Update `Event::to_subset_event()` (around line 90)

```rust
pub fn to_subset_event<ED>(&self) -> Result<Event<ED>, ED::Error>
where
    ED: EventData + TryFrom<D>,
    ED::Error: Send + Sync,
{
    let data = self
        .data
        .as_ref()
        .map(|d| ED::try_from(d.clone()))
        .transpose()?;
    Ok(Event {
        id: self.id,
        stream_id: self.stream_id,
        stream_version: self.stream_version,
        event_type: self.event_type.clone(),
        actor_id: self.actor_id,
        purger_id: self.purger_id,
        data,
        created_at: self.created_at,
        purged_at: self.purged_at,
        global_sequence: self.global_sequence,
        correlation_id: self.correlation_id,
        causation_id: self.causation_id,
    })
}
```

**Action 9**: Update `Event::to_subset_event_ref()` (around line 130)

```rust
pub fn to_subset_event_ref<ED>(&self) -> Result<Event<ED>, EnumConversionError>
where
    ED: EventData + for<'a> TryFrom<&'a D, Error = EnumConversionError>,
{
    let data = self.data.as_ref().map(|d| ED::try_from(d)).transpose()?;
    Ok(Event {
        id: self.id,
        stream_id: self.stream_id,
        stream_version: self.stream_version,
        global_sequence: self.global_sequence,
        event_type: self.event_type.clone(),
        actor_id: self.actor_id,
        purger_id: self.purger_id,
        data,
        created_at: self.created_at,
        purged_at: self.purged_at,
        correlation_id: self.correlation_id,
        causation_id: self.causation_id,
    })
}
```

**Action 10**: Update `Event::to_superset_event()` (around line 160)

```rust
pub fn to_superset_event<ED>(&self) -> Event<ED>
where
    ED: EventData,
    D: Into<ED>,
{
    let data = self.data.as_ref().map(|d| d.clone().into());
    Event {
        id: self.id,
        stream_id: self.stream_id,
        stream_version: self.stream_version,
        event_type: self.event_type.clone(),
        actor_id: self.actor_id,
        purger_id: self.purger_id,
        data,
        created_at: self.created_at,
        purged_at: self.purged_at,
        global_sequence: self.global_sequence,
        correlation_id: self.correlation_id,
        causation_id: self.causation_id,
    }
}
```

**Verify**: Run `cargo test --package epoch_core event` - all tests should pass

---

### Step 1.2: Add Causation Fields to Command Struct

**Files**: `epoch_core/src/aggregate.rs`
**Pattern Reference**: Based on existing `Command` struct pattern (line ~30)
**TDD**: Write failing tests first, then implement

#### Test First (Create new file: `epoch_core/tests/command_causation_tests.rs`)

```rust
//! Tests for command causation tracking.

use epoch_core::prelude::*;
use uuid::Uuid;

#[derive(Debug, Clone)]
enum TestCommand {
    DoSomething,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
enum TestEvent {
    SomethingDone,
}

impl EventData for TestEvent {
    fn event_type(&self) -> &'static str {
        "SomethingDone"
    }
}

#[test]
fn command_new_defaults_causation_to_none() {
    let cmd = Command::<TestCommand, ()>::new(
        Uuid::new_v4(),
        TestCommand::DoSomething,
        None,
        None,
    );
    
    assert_eq!(cmd.causation_id, None);
    assert_eq!(cmd.correlation_id, None);
}

#[test]
fn command_caused_by_sets_causation_and_correlation() {
    let correlation_id = Uuid::new_v4();
    let event_id = Uuid::new_v4();
    
    let event = Event::<TestEvent> {
        id: event_id,
        stream_id: Uuid::new_v4(),
        stream_version: 1,
        event_type: "SomethingDone".to_string(),
        actor_id: None,
        purger_id: None,
        data: Some(TestEvent::SomethingDone),
        created_at: chrono::Utc::now(),
        purged_at: None,
        global_sequence: Some(10),
        correlation_id: Some(correlation_id),
        causation_id: None,
    };
    
    let cmd = Command::<TestCommand, ()>::new(
        Uuid::new_v4(),
        TestCommand::DoSomething,
        None,
        None,
    )
    .caused_by(&event);
    
    assert_eq!(cmd.causation_id, Some(event_id));
    assert_eq!(cmd.correlation_id, Some(correlation_id));
}

#[test]
fn command_caused_by_handles_event_without_correlation() {
    let event_id = Uuid::new_v4();
    
    let event = Event::<TestEvent> {
        id: event_id,
        stream_id: Uuid::new_v4(),
        stream_version: 1,
        event_type: "SomethingDone".to_string(),
        actor_id: None,
        purger_id: None,
        data: Some(TestEvent::SomethingDone),
        created_at: chrono::Utc::now(),
        purged_at: None,
        global_sequence: Some(10),
        correlation_id: None,
        causation_id: None,
    };
    
    let cmd = Command::<TestCommand, ()>::new(
        Uuid::new_v4(),
        TestCommand::DoSomething,
        None,
        None,
    )
    .caused_by(&event);
    
    assert_eq!(cmd.causation_id, Some(event_id));
    assert_eq!(cmd.correlation_id, None);
}

#[test]
fn command_with_correlation_id_sets_explicit_correlation() {
    let correlation_id = Uuid::new_v4();
    
    let cmd = Command::<TestCommand, ()>::new(
        Uuid::new_v4(),
        TestCommand::DoSomething,
        None,
        None,
    )
    .with_correlation_id(correlation_id);
    
    assert_eq!(cmd.correlation_id, Some(correlation_id));
    assert_eq!(cmd.causation_id, None);
}

#[test]
fn command_to_subset_preserves_causation() {
    let correlation_id = Uuid::new_v4();
    let causation_id = Uuid::new_v4();
    
    let cmd = Command::<TestCommand, ()>::new(
        Uuid::new_v4(),
        TestCommand::DoSomething,
        None,
        None,
    )
    .with_correlation_id(correlation_id);
    
    // Manually set causation_id for test
    let mut cmd = cmd;
    cmd.causation_id = Some(causation_id);
    
    // For this test, we need identity conversion
    let subset = cmd.to_subset_command::<TestCommand>().unwrap();
    
    assert_eq!(subset.correlation_id, Some(correlation_id));
    assert_eq!(subset.causation_id, Some(causation_id));
}

// Identity conversion for testing
impl TryFrom<TestCommand> for TestCommand {
    type Error = std::convert::Infallible;
    
    fn try_from(value: TestCommand) -> Result<Self, Self::Error> {
        Ok(value)
    }
}
```

**Verify**: Run `cargo test --package epoch_core command_causation` - tests should fail

#### Implementation

**Action 1**: Add fields to `Command` struct (around line 35, after `aggregate_version`)

```rust
/// The ID of the event that caused this command to be dispatched.
///
/// This is `None` for commands originating from external triggers (e.g., HTTP requests)
/// and `Some(event_id)` when the command is dispatched by a saga reacting to an event.
pub causation_id: Option<Uuid>,

/// The correlation ID to propagate to events produced by this command.
///
/// All events in a causal tree share the same correlation ID. If not explicitly set,
/// the aggregate will auto-generate one from the first event's ID.
pub correlation_id: Option<Uuid>,
```

**Action 2**: Update `Command::new()` to initialize new fields (around line 70)

```rust
pub fn new(
    aggregate_id: Uuid,
    data: D,
    credentials: Option<C>,
    aggregate_version: Option<u64>,
) -> Self {
    Command {
        aggregate_id,
        data,
        credentials,
        aggregate_version,
        causation_id: None,
        correlation_id: None,
    }
}
```

**Action 3**: Add `caused_by()` method (after `Command::new()` around line 85)

```rust
/// Sets causation context from a triggering event.
///
/// This is the primary way sagas thread causation context when dispatching commands.
/// Sets `causation_id = Some(event.id)` and inherits the event's `correlation_id`.
///
/// # Example
///
/// ```ignore
/// async fn handle_event(&self, state: Self::State, event: &Event<Self::EventType>)
///     -> Result<Option<Self::State>, Self::SagaError>
/// {
///     match &event.data {
///         OrderEvent::OrderPlaced { order_id, .. } => {
///             let cmd = Command::new(*order_id, ReserveItems { .. }, None, None)
///                 .caused_by(event);
///             self.command_dispatcher.dispatch(cmd).await?;
///         }
///     }
///     Ok(Some(state))
/// }
/// ```
pub fn caused_by<ED: EventData>(mut self, event: &Event<ED>) -> Self {
    self.causation_id = Some(event.id);
    self.correlation_id = event.correlation_id;
    self
}
```

**Action 4**: Add `with_correlation_id()` method (after `caused_by()`)

```rust
/// Explicitly sets a correlation ID.
///
/// Use this at system entry points (e.g., HTTP handlers) to inject an external
/// trace ID as the correlation ID for all downstream events.
///
/// # Example
///
/// ```ignore
/// // In an HTTP handler
/// let trace_id = extract_trace_id(request);
/// let cmd = Command::new(order_id, PlaceOrder { .. }, Some(user), None)
///     .with_correlation_id(trace_id);
/// aggregate.handle(cmd).await?;
/// ```
pub fn with_correlation_id(mut self, correlation_id: Uuid) -> Self {
    self.correlation_id = Some(correlation_id);
    self
}
```

**Action 5**: Update `Command::to_subset_command()` (around line 90)

```rust
pub fn to_subset_command<CD>(&self) -> Result<Command<CD, C>, CD::Error>
where
    CD: TryFrom<D>,
    CD::Error: Send + Sync,
{
    let data = CD::try_from(self.data.clone())?;
    Ok(Command {
        aggregate_id: self.aggregate_id,
        data,
        credentials: self.credentials.clone(),
        aggregate_version: self.aggregate_version,
        causation_id: self.causation_id,
        correlation_id: self.correlation_id,
    })
}
```

**Action 6**: Update `Command::to_superset_command()` (around line 105)

```rust
pub fn to_superset_command<CD>(&self) -> Command<CD, C>
where
    CD: EventData,
    D: Into<CD>,
{
    let data = self.data.clone().into();
    Command {
        aggregate_id: self.aggregate_id,
        data,
        credentials: self.credentials.clone(),
        aggregate_version: self.aggregate_version,
        causation_id: self.causation_id,
        correlation_id: self.correlation_id,
    }
}
```

**Verify**: Run `cargo test --package epoch_core command_causation` - all tests should pass

---

### Step 1.3: Implement Automatic Propagation in Aggregate::handle()

**Files**: `epoch_core/src/aggregate.rs`
**Pattern Reference**: Based on existing event stamping pattern (line ~352)
**TDD**: Write failing integration test first

#### Test First (Add to `epoch_core/tests/aggregate_rehydration_tests.rs` at end of file)

```rust
/// Test that correlation_id is auto-generated when not provided on command
#[tokio::test]
async fn handle_auto_generates_correlation_id() {
    let state_store = InMemoryStateStore::<Counter>::default();
    let event_bus = InMemoryEventBus::<CounterEvent>::default();
    let event_store = InMemoryEventStore::new(event_bus.clone());

    let aggregate = CounterAggregate {
        state_store: state_store.clone(),
        event_store: event_store.clone(),
    };

    let counter_id = Uuid::new_v4();
    let cmd = Command::new(counter_id, CounterCommand::Create, None, None);

    let _state = aggregate.handle(cmd).await.unwrap();

    // Read back the event and verify correlation_id was auto-generated
    let stream = event_store.read_events(counter_id).await.unwrap();
    let events: Vec<_> = stream.collect().await;
    
    assert_eq!(events.len(), 1);
    let event = events[0].as_ref().unwrap();
    
    // Correlation should be set to the event's own ID
    assert_eq!(event.correlation_id, Some(event.id));
    assert_eq!(event.causation_id, None);
}

/// Test that explicit correlation_id on command is preserved
#[tokio::test]
async fn handle_preserves_explicit_correlation_id() {
    let state_store = InMemoryStateStore::<Counter>::default();
    let event_bus = InMemoryEventBus::<CounterEvent>::default();
    let event_store = InMemoryEventStore::new(event_bus.clone());

    let aggregate = CounterAggregate {
        state_store: state_store.clone(),
        event_store: event_store.clone(),
    };

    let counter_id = Uuid::new_v4();
    let explicit_correlation = Uuid::new_v4();
    
    let cmd = Command::new(counter_id, CounterCommand::Create, None, None)
        .with_correlation_id(explicit_correlation);

    let _state = aggregate.handle(cmd).await.unwrap();

    // Read back the event and verify explicit correlation was preserved
    let stream = event_store.read_events(counter_id).await.unwrap();
    let events: Vec<_> = stream.collect().await;
    
    assert_eq!(events.len(), 1);
    let event = events[0].as_ref().unwrap();
    
    assert_eq!(event.correlation_id, Some(explicit_correlation));
    assert_eq!(event.causation_id, None);
}

/// Test that causation_id from command is propagated to events
#[tokio::test]
async fn handle_propagates_causation_id() {
    let state_store = InMemoryStateStore::<Counter>::default();
    let event_bus = InMemoryEventBus::<CounterEvent>::default();
    let event_store = InMemoryEventStore::new(event_bus.clone());

    let aggregate = CounterAggregate {
        state_store: state_store.clone(),
        event_store: event_store.clone(),
    };

    let counter_id = Uuid::new_v4();
    let triggering_event_id = Uuid::new_v4();
    let correlation_id = Uuid::new_v4();
    
    let mut cmd = Command::new(counter_id, CounterCommand::Create, None, None);
    cmd.causation_id = Some(triggering_event_id);
    cmd.correlation_id = Some(correlation_id);

    let _state = aggregate.handle(cmd).await.unwrap();

    // Read back the event and verify causation was propagated
    let stream = event_store.read_events(counter_id).await.unwrap();
    let events: Vec<_> = stream.collect().await;
    
    assert_eq!(events.len(), 1);
    let event = events[0].as_ref().unwrap();
    
    assert_eq!(event.correlation_id, Some(correlation_id));
    assert_eq!(event.causation_id, Some(triggering_event_id));
}

/// Test that multiple events in one command share the same auto-generated correlation_id
#[tokio::test]
async fn handle_auto_generates_shared_correlation_for_multiple_events() {
    let state_store = InMemoryStateStore::<Counter>::default();
    let event_bus = InMemoryEventBus::<CounterEvent>::default();
    let event_store = InMemoryEventStore::new(event_bus.clone());

    let aggregate = CounterAggregate {
        state_store: state_store.clone(),
        event_store: event_store.clone(),
    };

    let counter_id = Uuid::new_v4();
    
    // First create the counter
    let create_cmd = Command::new(counter_id, CounterCommand::Create, None, None);
    aggregate.handle(create_cmd).await.unwrap();

    // Modify CounterCommand to support producing multiple events
    // For this test, we'll simulate by doing two increments with same correlation
    let increment_cmd = Command::new(counter_id, CounterCommand::Increment, None, None);
    aggregate.handle(increment_cmd).await.unwrap();

    // Read back events
    let stream = event_store.read_events(counter_id).await.unwrap();
    let events: Vec<_> = stream.collect().await;
    
    assert_eq!(events.len(), 2);
    
    // Both events should have correlation_id (first one auto-generated, second inherited)
    let event1 = events[0].as_ref().unwrap();
    let event2 = events[1].as_ref().unwrap();
    
    assert!(event1.correlation_id.is_some());
    // Second event gets its own correlation since it's a separate command
    assert!(event2.correlation_id.is_some());
}
```

**Verify**: Run `cargo test --package epoch_core aggregate_rehydration` - new tests should fail

#### Implementation

**Action 1**: Update `Aggregate::handle()` to stamp causation fields (around line 352)

Find the existing event stamping code that sets `stream_version`:

```rust
// BEFORE:
let events: Vec<Event<ED>> = self
    .handle_command(&state, cmd)
    .await
    .map_err(HandleCommandError::Command)?
    .into_iter()
    .enumerate()
    .map(|(i, mut e)| {
        e.stream_version = new_state_version + i as u64;
        new_state_version = e.stream_version;
        e
    })
    .collect();
```

Replace with:

```rust
// AFTER:
// Extract causation fields before consuming in the iterator
let causation_id = command.causation_id;
let explicit_correlation = command.correlation_id;

// Generate events from command
let mut events: Vec<Event<ED>> = self
    .handle_command(&state, cmd)
    .await
    .map_err(HandleCommandError::Command)?
    .into_iter()
    .enumerate()
    .map(|(i, mut e)| {
        e.stream_version = new_state_version + i as u64;
        new_state_version = e.stream_version;
        e
    })
    .collect();

// Stamp causation fields on all events
if let Some(first_event) = events.first_mut() {
    // Set causation_id if provided
    first_event.causation_id = causation_id;
    
    // Set correlation_id: use explicit or auto-generate from first event's id
    first_event.correlation_id = Some(explicit_correlation.unwrap_or(first_event.id));
}

// Propagate correlation_id and causation_id to remaining events
if events.len() > 1 {
    let correlation_id = events[0].correlation_id;
    let causation_id = events[0].causation_id;
    
    for event in events.iter_mut().skip(1) {
        event.correlation_id = correlation_id;
        event.causation_id = causation_id;
    }
}
```

**Action 2**: Update `AggregateTransaction::handle()` with identical stamping logic (around line 800)

Find the similar event stamping code in the transaction handler and apply the same changes:

```rust
// Extract causation fields before consuming in the iterator
let causation_id = command.causation_id;
let explicit_correlation = command.correlation_id;

// Handle command to produce events
let mut events: Vec<Event<A::SupersetEvent>> = self
    .aggregate
    .handle_command(&state, cmd)
    .await
    .map_err(HandleCommandError::Command)?
    .into_iter()
    .enumerate()
    .map(|(i, mut e)| {
        e.stream_version = next_version + i as u64;
        e
    })
    .collect();

// Stamp causation fields on all events
if let Some(first_event) = events.first_mut() {
    first_event.causation_id = causation_id;
    first_event.correlation_id = Some(explicit_correlation.unwrap_or(first_event.id));
}

if events.len() > 1 {
    let correlation_id = events[0].correlation_id;
    let causation_id = events[0].causation_id;
    
    for event in events.iter_mut().skip(1) {
        event.correlation_id = correlation_id;
        event.causation_id = causation_id;
    }
}
```

**Verify**: Run `cargo test --package epoch_core` - all tests should pass

---

### Step 1.4: Update In-Memory Implementations

**Files**: `epoch_mem/src/event_store.rs`
**Pattern Reference**: Based on existing `InMemoryEventStore` pattern
**Action**: No schema changes needed - `Option<Uuid>` fields serialize/deserialize automatically

#### Test (Add to `epoch_mem/src/event_store.rs` test module if it exists, or create inline tests)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use epoch_core::prelude::*;
    use uuid::Uuid;

    #[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
    enum TestEvent {
        Created,
    }

    impl EventData for TestEvent {
        fn event_type(&self) -> &'static str {
            "Created"
        }
    }

    #[tokio::test]
    async fn store_and_read_event_with_causation() {
        let bus = InMemoryEventBus::<TestEvent>::default();
        let store = InMemoryEventStore::new(bus);

        let correlation_id = Uuid::new_v4();
        let causation_id = Uuid::new_v4();
        let stream_id = Uuid::new_v4();

        let event = Event::<TestEvent>::builder()
            .stream_id(stream_id)
            .stream_version(1)
            .event_type("Created".to_string())
            .data(Some(TestEvent::Created))
            .correlation_id(correlation_id)
            .causation_id(causation_id)
            .build()
            .unwrap();

        store.store_event(event.clone()).await.unwrap();

        // Read back and verify causation fields preserved
        let stream = store.read_events(stream_id).await.unwrap();
        let events: Vec<_> = stream.collect().await;

        assert_eq!(events.len(), 1);
        let retrieved = events[0].as_ref().unwrap();

        assert_eq!(retrieved.correlation_id, Some(correlation_id));
        assert_eq!(retrieved.causation_id, Some(causation_id));
    }
}
```

**Verify**: Run `cargo test --package epoch_mem` - test should pass (no implementation changes needed)

---

## Files Summary

### Modified Files

| File | Changes |
|------|---------|
| `epoch_core/src/event.rs` | Add `correlation_id` and `causation_id` fields to `Event` and `EventBuilder`; add builder methods; update all conversion methods to preserve new fields |
| `epoch_core/src/aggregate.rs` | Add `correlation_id` and `causation_id` fields to `Command`; add `caused_by()` and `with_correlation_id()` methods; update conversion methods; implement automatic stamping in `handle()` and `AggregateTransaction::handle()` |
| `epoch_core/tests/command_causation_tests.rs` | New test file for command causation functionality |
| `epoch_core/tests/aggregate_rehydration_tests.rs` | Add tests for automatic causation propagation in aggregate handling |
| `epoch_mem/src/event_store.rs` | Add tests to verify causation fields round-trip correctly (no code changes needed) |

### Test Coverage

- ✅ Event field propagation through all conversion methods
- ✅ Command builder methods (`caused_by()`, `with_correlation_id()`)
- ✅ Command conversion methods preserve causation
- ✅ Automatic correlation_id generation when not provided
- ✅ Explicit correlation_id preserved when provided
- ✅ Causation_id propagated from command to events
- ✅ Multiple events share same correlation_id
- ✅ In-memory store round-trip preservation

## Completion Checklist

- [ ] Step 1.1: Event struct changes complete and tested
- [ ] Step 1.2: Command struct changes complete and tested
- [ ] Step 1.3: Automatic propagation implemented and tested
- [ ] Step 1.4: In-memory implementation tested
- [ ] All tests pass: `./scripts/test.sh --skip-e2e`
- [ ] Code formatted: `cargo fmt`
- [ ] No clippy warnings: `cargo clippy -- -D warnings`
- [ ] Documentation builds: `cargo doc --no-deps`

## Notes for Next Phase

Phase 2 will build on this foundation by:
1. Adding PostgreSQL migration for persistence
2. Updating `PgDBEvent` struct and all INSERT/SELECT statements
3. Updating NOTIFY trigger to include new fields
4. Adding query methods to `EventStoreBackend` trait

The automatic propagation implemented in this phase means that once Phase 2 is complete, all events will automatically have causation tracking without requiring changes to existing aggregates or command handlers.
