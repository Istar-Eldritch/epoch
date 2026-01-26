//! This file should fail to compile, verifying the anti-pattern is prevented.
//!
//! The test verifies that an Aggregate cannot be used with ProjectionHandler
//! when subscribing to an event bus, because Aggregate extends EventApplicator
//! but NOT Projection.

use epoch_core::prelude::*;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

// Minimal event data
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
enum TestEvent {
    Created,
}

impl EventData for TestEvent {
    fn event_type(&self) -> &'static str {
        "TestEvent"
    }
}

impl TryFrom<&TestEvent> for TestEvent {
    type Error = epoch_core::event::EnumConversionError;
    fn try_from(value: &TestEvent) -> Result<Self, Self::Error> {
        Ok(value.clone())
    }
}

// Minimal state
#[derive(Debug, Clone)]
struct TestState {
    id: Uuid,
    version: u64,
}

impl EventApplicatorState for TestState {
    fn get_id(&self) -> &Uuid {
        &self.id
    }
}

impl AggregateState for TestState {
    fn get_version(&self) -> u64 {
        self.version
    }
    fn set_version(&mut self, version: u64) {
        self.version = version;
    }
}

// Minimal state store
#[derive(Debug, Clone)]
struct TestStateStore(Arc<Mutex<HashMap<Uuid, TestState>>>);

#[derive(Debug, thiserror::Error)]
enum TestStateStoreError {}

#[async_trait::async_trait]
impl StateStoreBackend<TestState> for TestStateStore {
    type Error = TestStateStoreError;
    async fn get_state(&self, _id: Uuid) -> Result<Option<TestState>, Self::Error> {
        Ok(None)
    }
    async fn persist_state(&mut self, _id: Uuid, _state: TestState) -> Result<(), Self::Error> {
        Ok(())
    }
    async fn delete_state(&mut self, _id: Uuid) -> Result<(), Self::Error> {
        Ok(())
    }
}

// Minimal event store
#[derive(Debug, Clone)]
struct TestEventStore;

#[derive(Debug, thiserror::Error)]
enum TestEventStoreError {}

#[async_trait::async_trait]
impl EventStoreBackend for TestEventStore {
    type Error = TestEventStoreError;
    type EventType = TestEvent;

    async fn read_events(
        &self,
        _stream_id: Uuid,
    ) -> Result<Pin<Box<dyn EventStream<Self::EventType, Self::Error> + Send + 'life0>>, Self::Error>
    {
        unimplemented!()
    }

    async fn read_events_since(
        &self,
        _stream_id: Uuid,
        _version: u64,
    ) -> Result<Pin<Box<dyn EventStream<Self::EventType, Self::Error> + Send + 'life0>>, Self::Error>
    {
        unimplemented!()
    }

    async fn store_event(&self, _event: Event<Self::EventType>) -> Result<(), Self::Error> {
        Ok(())
    }
}

// Aggregate implementation (NOT a Projection)
struct TestAggregate {
    state_store: TestStateStore,
    event_store: TestEventStore,
}

#[derive(Debug, thiserror::Error)]
enum TestApplyError {}

impl EventApplicator<TestEvent> for TestAggregate {
    type State = TestState;
    type StateStore = TestStateStore;
    type EventType = TestEvent;
    type ApplyError = TestApplyError;

    fn apply(
        &self,
        _state: Option<Self::State>,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::ApplyError> {
        Ok(Some(TestState {
            id: event.stream_id,
            version: event.stream_version,
        }))
    }

    fn get_state_store(&self) -> Self::StateStore {
        self.state_store.clone()
    }
}

#[derive(Debug, Clone)]
enum TestCommand {
    Create,
}

#[derive(Debug, thiserror::Error)]
enum TestAggregateError {}

#[async_trait::async_trait]
impl Aggregate<TestEvent> for TestAggregate {
    type CommandData = TestCommand;
    type CommandCredentials = ();
    type Command = TestCommand;
    type EventStore = TestEventStore;
    type AggregateError = TestAggregateError;

    fn get_event_store(&self) -> Self::EventStore {
        self.event_store.clone()
    }

    async fn handle_command(
        &self,
        _state: &Option<Self::State>,
        _command: Command<Self::Command, Self::CommandCredentials>,
    ) -> Result<Vec<Event<TestEvent>>, Self::AggregateError> {
        Ok(vec![])
    }
}

// Minimal event bus for testing
struct TestEventBus;

#[derive(Debug, thiserror::Error)]
enum TestEventBusError {}

impl EventBus for TestEventBus {
    type Error = TestEventBusError;
    type EventType = TestEvent;

    fn publish<'a>(
        &'a self,
        _event: Arc<Event<Self::EventType>>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }

    fn subscribe<T>(
        &self,
        _observer: T,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), Self::Error>> + Send>>
    where
        T: EventObserver<Self::EventType> + Send + Sync + 'static,
    {
        Box::pin(async { Ok(()) })
    }
}

fn main() {
    let aggregate = TestAggregate {
        state_store: TestStateStore(Arc::new(Mutex::new(HashMap::new()))),
        event_store: TestEventStore,
    };

    let handler = ProjectionHandler::new(aggregate);

    // This should fail to compile: TestAggregate doesn't implement Projection,
    // so ProjectionHandler<TestAggregate> doesn't implement EventObserver<TestEvent>
    let bus = TestEventBus;
    let _ = bus.subscribe(handler);
}
