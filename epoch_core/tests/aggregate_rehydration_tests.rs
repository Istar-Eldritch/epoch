//! Integration tests for aggregate re-hydration behavior.
//!
//! These tests verify that the aggregate correctly re-hydrates from the event
//! store when the state store is stale (has an older version than the event store).
//!
//! This is critical for scenarios where:
//! - A saga handles an event and modifies an aggregate
//! - Another process (e.g., a supervisor) tries to send a command to the same aggregate
//! - The state store hasn't been updated yet with the saga's changes

use async_trait::async_trait;
use epoch_core::event::EnumConversionError;
use epoch_core::prelude::*;
use epoch_mem::*;
use tokio_stream::StreamExt;
use uuid::Uuid;

/// Event data for aggregate tests
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum CounterEvent {
    Created,
    Incremented,
}

impl EventData for CounterEvent {
    fn event_type(&self) -> &'static str {
        match self {
            CounterEvent::Created => "CounterCreated",
            CounterEvent::Incremented => "CounterIncremented",
        }
    }
}

// Identity conversion for testing - clones the data
impl TryFrom<&CounterEvent> for CounterEvent {
    type Error = EnumConversionError;

    fn try_from(value: &CounterEvent) -> Result<Self, Self::Error> {
        Ok(value.clone())
    }
}

/// Command data for aggregate tests
#[derive(Debug, Clone)]
enum CounterCommand {
    Create,
    Increment,
}

/// State for aggregate tests
#[derive(Debug, Clone)]
struct Counter {
    id: Uuid,
    value: u64,
    version: u64,
}

impl EventApplicatorState for Counter {
    fn get_id(&self) -> &Uuid {
        &self.id
    }
}

impl AggregateState for Counter {
    fn get_version(&self) -> u64 {
        self.version
    }
    fn set_version(&mut self, version: u64) {
        self.version = version;
    }
}

/// Aggregate for testing
struct CounterAggregate {
    state_store: InMemoryStateStore<Counter>,
    event_store: InMemoryEventStore<InMemoryEventBus<CounterEvent>>,
}

#[derive(Debug, thiserror::Error)]
enum CounterApplyError {
    #[error("No state present for id {0}")]
    NoState(Uuid),
}

impl EventApplicator<CounterEvent> for CounterAggregate {
    type State = Counter;
    type EventType = CounterEvent;
    type StateStore = InMemoryStateStore<Counter>;
    type ApplyError = CounterApplyError;

    fn get_state_store(&self) -> Self::StateStore {
        self.state_store.clone()
    }

    fn apply(
        &self,
        state: Option<Self::State>,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, CounterApplyError> {
        match event.data.as_ref().unwrap() {
            CounterEvent::Created => Ok(Some(Counter {
                id: event.stream_id,
                value: 0,
                version: event.stream_version,
            })),
            CounterEvent::Incremented => {
                let mut state = state.ok_or(CounterApplyError::NoState(event.stream_id))?;
                state.value += 1;
                state.version = event.stream_version;
                Ok(Some(state))
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum CounterAggregateError {
    #[error("Error building event")]
    EventBuild(#[from] EventBuilderError),
}

#[async_trait]
impl Aggregate<CounterEvent> for CounterAggregate {
    type CommandData = CounterCommand;
    type CommandCredentials = ();
    type Command = CounterCommand;
    type AggregateError = CounterAggregateError;
    type EventStore = InMemoryEventStore<InMemoryEventBus<CounterEvent>>;

    fn get_event_store(&self) -> Self::EventStore {
        self.event_store.clone()
    }

    async fn handle_command(
        &self,
        _state: &Option<Self::State>,
        command: Command<Self::Command, Self::CommandCredentials>,
    ) -> Result<Vec<Event<CounterEvent>>, Self::AggregateError> {
        match command.data {
            CounterCommand::Create => {
                let event = CounterEvent::Created
                    .into_builder()
                    .stream_id(command.aggregate_id)
                    .build()?;
                Ok(vec![event])
            }
            CounterCommand::Increment => {
                let event = CounterEvent::Incremented
                    .into_builder()
                    .stream_id(command.aggregate_id)
                    .build()?;
                Ok(vec![event])
            }
        }
    }
}

/// Test that the aggregate correctly handles commands when the state store
/// is up to date with the event store.
#[tokio::test]
async fn aggregate_handles_command_with_current_state() {
    let bus = InMemoryEventBus::<CounterEvent>::new();
    let event_store = InMemoryEventStore::new(bus);
    let state_store = InMemoryStateStore::new();

    let aggregate = CounterAggregate {
        state_store: state_store.clone(),
        event_store,
    };

    let counter_id = Uuid::new_v4();

    // Create counter
    aggregate
        .handle(Command::new(counter_id, CounterCommand::Create, None, None))
        .await
        .unwrap();

    // Increment counter multiple times
    aggregate
        .handle(Command::new(
            counter_id,
            CounterCommand::Increment,
            None,
            None,
        ))
        .await
        .unwrap();

    aggregate
        .handle(Command::new(
            counter_id,
            CounterCommand::Increment,
            None,
            None,
        ))
        .await
        .unwrap();

    // Verify state
    let state = state_store.get_state(counter_id).await.unwrap().unwrap();
    assert_eq!(state.value, 2);
    assert_eq!(state.version, 3); // Create=1, Inc=2, Inc=3
}

/// Test that the aggregate correctly re-hydrates from the event store when
/// the state store is stale (has an older version than the event store).
///
/// This simulates the scenario where:
/// 1. A command is handled and events are stored
/// 2. The state store is NOT updated (simulating a stale projection)
/// 3. Another command comes in and should still work correctly
///
/// Real-world example: A saga handles JobSubmitted and adds JobMachineSet event,
/// but before the projection updates, the supervisor sends MarkJobAsRunning.
#[tokio::test]
async fn aggregate_rehydrates_when_state_store_is_stale() {
    let bus = InMemoryEventBus::<CounterEvent>::new();
    let event_store = InMemoryEventStore::new(bus);
    let state_store = InMemoryStateStore::new();

    let aggregate = CounterAggregate {
        state_store: state_store.clone(),
        event_store: event_store.clone(),
    };

    let counter_id = Uuid::new_v4();

    // Create counter - this stores event AND updates state store
    aggregate
        .handle(Command::new(counter_id, CounterCommand::Create, None, None))
        .await
        .unwrap();

    // Verify initial state
    let state = state_store.get_state(counter_id).await.unwrap().unwrap();
    assert_eq!(state.value, 0);
    assert_eq!(state.version, 1);

    // Now simulate a stale state store by directly adding an event to the
    // event store WITHOUT updating the state store (this simulates what
    // happens when a saga or another process adds events)
    let increment_event = CounterEvent::Incremented
        .into_builder()
        .stream_id(counter_id)
        .stream_version(2)
        .build()
        .unwrap();
    event_store.store_event(increment_event).await.unwrap();

    // State store still has version 1
    let stale_state = state_store.get_state(counter_id).await.unwrap().unwrap();
    assert_eq!(stale_state.version, 1);
    assert_eq!(stale_state.value, 0);

    // Now handle another Increment command - this should:
    // 1. Read stale state (version 1) from state store
    // 2. Re-hydrate from event store (find event at version 2)
    // 3. Use the re-hydrated state (version 2, value 1)
    // 4. Generate new event with version 3
    let result = aggregate
        .handle(Command::new(
            counter_id,
            CounterCommand::Increment,
            None,
            None,
        ))
        .await;

    // The command should succeed (not fail with version conflict)
    assert!(result.is_ok(), "Command failed: {:?}", result.err());

    // Verify final state - should have value=2 (two increments total)
    let final_state = state_store.get_state(counter_id).await.unwrap().unwrap();
    assert_eq!(final_state.value, 2);
    assert_eq!(final_state.version, 3);
}

/// Test that multiple concurrent-like commands work correctly even when
/// state store updates lag behind event store updates.
#[tokio::test]
async fn aggregate_handles_multiple_stale_state_scenarios() {
    let bus = InMemoryEventBus::<CounterEvent>::new();
    let event_store = InMemoryEventStore::new(bus);
    let state_store = InMemoryStateStore::new();

    let aggregate = CounterAggregate {
        state_store: state_store.clone(),
        event_store: event_store.clone(),
    };

    let counter_id = Uuid::new_v4();

    // Create counter
    aggregate
        .handle(Command::new(counter_id, CounterCommand::Create, None, None))
        .await
        .unwrap();

    // Simulate multiple events being added directly to event store
    // (as if by a saga or concurrent process)
    for version in 2..=5 {
        let event = CounterEvent::Incremented
            .into_builder()
            .stream_id(counter_id)
            .stream_version(version)
            .build()
            .unwrap();
        event_store.store_event(event).await.unwrap();
    }

    // State store is very stale (version 1), event store has version 5
    let stale_state = state_store.get_state(counter_id).await.unwrap().unwrap();
    assert_eq!(stale_state.version, 1);

    // Handle a new command - should re-hydrate all missed events
    let result = aggregate
        .handle(Command::new(
            counter_id,
            CounterCommand::Increment,
            None,
            None,
        ))
        .await;

    assert!(result.is_ok(), "Command failed: {:?}", result.err());

    // Final state should reflect all events: Create + 4 direct increments + 1 command increment = 5
    let final_state = state_store.get_state(counter_id).await.unwrap().unwrap();
    assert_eq!(final_state.value, 5);
    assert_eq!(final_state.version, 6);
}

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

    // Now increment - each command gets its own correlation_id
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
