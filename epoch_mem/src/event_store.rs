use async_trait::async_trait;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::Mutex;
use uuid::Uuid;

use futures_core::Stream;

use epoch_core::prelude::*;

/// The in-memory data store.
#[derive(Debug)]
struct EventStoreData<D: EventData> {
    events: HashMap<Uuid, Event<D>>,
    stream_events: HashMap<Uuid, Vec<Uuid>>,
    stream_version: HashMap<Uuid, u64>,
}

/// An in-memory event store.
///
/// This event store is useful for testing and development purposes. It is not recommended for
/// production use, as it does not persist events to any durable storage.
#[derive(Clone, Debug)]
pub struct InMemoryEventStore<B: EventBus + Clone> {
    data: Arc<Mutex<EventStoreData<B::EventType>>>,
    bus: B,
}

impl<B: EventBus + Clone> InMemoryEventStore<B> {
    /// Creates a new `MemEventStore`.
    pub fn new(bus: B) -> Self {
        log::debug!("Creating a new InMemoryEventStore");
        Self {
            data: Arc::new(Mutex::new(EventStoreData {
                events: HashMap::new(),
                stream_events: HashMap::new(),
                stream_version: HashMap::new(),
            })),
            bus,
        }
    }

    /// Exposes the event store bus
    pub fn bus(&self) -> &B {
        &self.bus
    }
}

/// Errors returned by the InMemoryEventBus
#[derive(Debug, thiserror::Error)]
pub enum InMemoryEventStoreBackendError {
    ///
    #[error("Event version ({0}) doesn't match stream version ({1})")]
    VersionMismatch(u64, u64),
    /// Error publishing event to bus
    #[error("Error publishing event to bus")]
    PublishEvent,
}

#[async_trait]
impl<B> EventStoreBackend for InMemoryEventStore<B>
where
    B: EventBus + Send + Sync + Clone,
{
    type Error = InMemoryEventStoreBackendError;
    type EventType = B::EventType;

    async fn read_events(
        &self,
        stream_id: Uuid,
    ) -> Result<Pin<Box<dyn EventStream<Self::EventType> + Send + 'life0>>, Self::Error> {
        log::debug!("Reading events for stream_id: {}", stream_id);
        let data = self.data.clone();
        let stream: Pin<Box<dyn EventStream<Self::EventType> + Send>> =
            Box::pin(InMemoryEventStoreStream::<B>::new(data, stream_id));
        Ok(stream)
    }

    async fn store_event(&self, event: Event<Self::EventType>) -> Result<(), Self::Error> {
        let data = self.data.clone();
        let bus = self.bus.clone();
        let mut data = data.lock().await;
        let stream_id = event.stream_id;

        let version: u64 = *data.stream_version.get(&stream_id).unwrap_or(&0);

        if event.stream_version != version {
            log::debug!(
                "Event version mismatch for stream_id: {}. Expected: {}, Got: {}",
                stream_id,
                version,
                event.stream_version
            );
            return Err(InMemoryEventStoreBackendError::VersionMismatch(
                event.stream_version,
                version,
            ));
        }
        log::debug!(
            "Event version check passed for stream_id: {}. Version: {}",
            stream_id,
            version
        );

        let new_version = version + 1;

        data.stream_version.insert(stream_id, new_version);

        let event_id = event.id;
        data.events.insert(event_id, event.clone());
        data.stream_events
            .entry(stream_id)
            .or_default()
            .extend(&[event_id]);
        log::debug!(
            "Event stored successfully for stream_id: {}, event_id: {}",
            stream_id,
            event_id
        );
        if bus.publish(event.clone()).await.is_err() {
            return Err(InMemoryEventStoreBackendError::PublishEvent);
        }
        Ok(())
    }
}

/// An in-memory event store stream.
pub struct InMemoryEventStoreStream<B>
where
    B: EventBus + Clone,
{
    id: Uuid,
    data: Arc<Mutex<EventStoreData<B::EventType>>>,
    current_index: usize,
    _phantom: PhantomData<B>,
}

impl<B> InMemoryEventStoreStream<B>
where
    B: EventBus + Clone,
{
    fn new(data: Arc<Mutex<EventStoreData<B::EventType>>>, id: Uuid) -> Self {
        Self {
            data,
            id,
            current_index: 0,
            _phantom: PhantomData,
        }
    }
}

impl<B> EventStream<B::EventType> for InMemoryEventStoreStream<B> where
    B: EventBus + Clone + Send + Sync
{
}

impl<B> Stream for InMemoryEventStoreStream<B>
where
    B: EventBus + Clone + Send + Sync,
    B::EventType: Send + Sync,
{
    type Item = Result<Event<B::EventType>, Box<dyn std::error::Error + Send + Sync>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // We need to use unsafe to get a mutable reference to the fields of the `!Unpin` struct.
        // This is safe because we are not moving the `VirtualEventStoreStream` itself.
        let this = unsafe { self.get_unchecked_mut() };

        let data = match this.data.try_lock() {
            Ok(guard) => {
                log::debug!("InMemoryEventStoreStream: poll_next - Acquired lock");
                guard
            }
            Err(_) => {
                log::debug!(
                    "InMemoryEventStoreStream: poll_next - Lock contention, returning Poll::Pending"
                );
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
        };

        if let Some(event_ids) = data.stream_events.get(&this.id) {
            log::debug!(
                "InMemoryEventStoreStream: poll_next - Found {} events for stream_id: {}",
                event_ids.len(),
                this.id
            );
            while this.current_index < event_ids.len() {
                let event_id = event_ids[this.current_index];
                this.current_index += 1;

                // Find the actual event in the store's main events vector
                if let Some(event) = data.events.get(&event_id) {
                    log::debug!(
                        "InMemoryEventStoreStream: poll_next - Returning event_id: {}",
                        event_id
                    );
                    return Poll::Ready(Some(Ok(event.clone())));
                }
            }
            log::debug!(
                "InMemoryEventStoreStream: poll_next - No more events in stream, returning Poll::Ready(None)"
            );
        } else {
            log::debug!(
                "InMemoryEventStoreStream: poll_next - No events found for stream_id: {}",
                this.id
            );
        }

        Poll::Ready(None)
    }
}

//
// /// The error for the InMemoryEventBus event publication
// #[derive(Debug, thiserror::Error)]
// #[error("In memory eventbus publish error")]
// pub struct InMemoryEventBusPublishError;
//
/// An implementation of an in-memory event bus
#[derive(Clone)]
pub struct InMemoryEventBus<D>
where
    D: EventData + Send + Sync + 'static,
{
    _phantom: PhantomData<D>,
    projections: Arc<Mutex<Vec<Arc<Mutex<dyn EventObserver<D>>>>>>,
}

impl<D> InMemoryEventBus<D>
where
    D: EventData + Send + Sync,
{
    /// Creates a new in-memory bus
    pub fn new() -> Self {
        log::debug!("Creating a new InMemoryEventBus");
        Self {
            _phantom: PhantomData,
            projections: Arc::new(Mutex::new(vec![])),
        }
    }
}

impl<D> std::fmt::Debug for InMemoryEventBus<D>
where
    D: EventData + Send + Sync,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryEventBus").finish()
    }
}

/// Errors for the InMemoryEventBus
#[derive(Debug, thiserror::Error)]
pub enum InMemoryEventBusError {}

impl<D> EventBus for InMemoryEventBus<D>
where
    D: EventData + Send + Sync + 'static,
{
    type Error = InMemoryEventBusError;
    type EventType = D;
    fn publish<'a>(
        &'a self,
        event: Event<Self::EventType>,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async move {
            log::debug!("Publishing event with id: {}", event.id);
            let projections = self.projections.lock().await;
            for projection in projections.iter() {
                let projection = projection.lock().await;
                projection
                    .on_event(event.clone())
                    .await
                    .unwrap_or_else(|e| {
                        log::error!("Error applying event: {:?}", e);
                        //TODO: Retry mechanism and dead letter queue
                    });
            }
            Ok(())
        })
    }

    fn subscribe<T>(
        &self,
        projector: T,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send>>
    where
        T: EventObserver<Self::EventType> + Send + Sync + 'static,
    {
        log::debug!("Subscribing projector to InMemoryEventBus");
        let projectors = self.projections.clone();
        Box::pin(async move {
            let mut projectors = projectors.lock().await;
            projectors.push(Arc::new(Mutex::new(projector)));
            Ok(())
        })
    }
}

/// Implementation of an InMemory State Storage
#[derive(Clone, Debug)]
pub struct InMemoryStateStore<T>(std::sync::Arc<Mutex<HashMap<Uuid, T>>>)
where
    T: Clone + std::fmt::Debug;

impl<T> InMemoryStateStore<T>
where
    T: Clone + std::fmt::Debug,
{
    /// Creates a new InMemoryStateStore
    pub fn new() -> Self {
        InMemoryStateStore(Arc::new(Mutex::new(HashMap::new())))
    }
}

#[async_trait]
impl<T> StateStoreBackend<T> for InMemoryStateStore<T>
where
    T: Clone + Send + Sync + std::fmt::Debug,
{
    async fn get_state(
        &self,
        id: Uuid,
    ) -> Result<Option<T>, Box<dyn std::error::Error + Send + Sync>> {
        let data = self.0.lock().await;
        Ok(data.get(&id).map(|v| v.clone()))
    }
    async fn persist_state(
        &mut self,
        id: Uuid,
        state: T,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut data = self.0.lock().await;
        data.insert(id, state);
        Ok(())
    }
    async fn delete_state(
        &mut self,
        id: Uuid,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut data = self.0.lock().await;
        data.remove(&id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use epoch_core::event::EventData;
    use tokio_stream::StreamExt;
    use uuid::Uuid;

    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    struct MyEventData {
        value: String,
    }

    impl EventData for MyEventData {
        fn event_type(&self) -> &'static str {
            "MyEvent"
        }
    }

    // Helper function to create a new event
    fn new_event(stream_id: Uuid, stream_version: u64, value: &str) -> Event<MyEventData> {
        Event::<MyEventData>::builder()
            .stream_id(stream_id)
            .event_type("MyEvent".to_string())
            .stream_version(stream_version)
            .data(Some(MyEventData {
                value: value.to_string(),
            }))
            .build()
            .unwrap()
    }

    struct TestProjection(InMemoryStateStore<TestState>);

    impl TestProjection {
        pub fn new() -> Self {
            TestProjection(InMemoryStateStore::new())
        }
    }

    #[derive(Debug, Clone)]
    struct TestState(Vec<Event<MyEventData>>);

    impl ProjectionState for TestState {
        fn get_version(&self) -> u64 {
            0
        }
        fn get_id(&self) -> Uuid {
            Uuid::new_v4()
        }
    }

    #[async_trait]
    impl Projection<MyEventData> for TestProjection {
        type State = TestState;
        type StateStore = InMemoryStateStore<Self::State>;
        type CreateEvent = MyEventData;
        type UpdateEvent = MyEventData;
        type DeleteEvent = MyEventData;
        fn get_state_store(&self) -> Self::StateStore {
            self.0.clone()
        }
        fn apply_create(
            &self,
            event: &Event<Self::CreateEvent>,
        ) -> Result<Self::State, Box<dyn std::error::Error + Send + Sync>> {
            Ok(TestState(vec![event.clone()]))
        }
        fn apply_update(
            &self,
            mut state: Self::State,
            event: &Event<Self::CreateEvent>,
        ) -> Result<Self::State, Box<dyn std::error::Error + Send + Sync>> {
            state.0.push(event.clone());
            Ok(state)
        }
    }

    #[tokio::test]
    async fn in_memory_event_store_new() {
        let bus = InMemoryEventBus::<MyEventData>::new();
        let store = InMemoryEventStore::new(bus.clone());
        assert!(store.data.lock().await.events.is_empty());
        assert!(store.data.lock().await.stream_events.is_empty());
        assert!(store.data.lock().await.stream_version.is_empty());
        assert!(store.bus().projections.lock().await.is_empty());
    }

    #[tokio::test]
    async fn in_memory_event_store_store_event() {
        let bus = InMemoryEventBus::<MyEventData>::new();
        let store = InMemoryEventStore::new(bus);
        let stream_id = Uuid::new_v4();

        let event1 = new_event(stream_id, 0, "test1");
        store.store_event(event1.clone()).await.unwrap();

        let event2 = new_event(stream_id, 1, "test2");
        store.store_event(event2.clone()).await.unwrap();

        let data = store.data.lock().await;
        assert_eq!(data.events.len(), 2);
        assert_eq!(data.stream_events.get(&stream_id).unwrap().len(), 2);
        assert_eq!(*data.stream_version.get(&stream_id).unwrap(), 2);
    }

    #[tokio::test]
    async fn in_memory_event_store_store_event_version_mismatch() {
        let bus = InMemoryEventBus::<MyEventData>::new();
        let store = InMemoryEventStore::new(bus);
        let stream_id = Uuid::new_v4();

        let event1 = new_event(stream_id, 0, "test1");
        store.store_event(event1.clone()).await.unwrap();

        let event_mismatch = new_event(stream_id, 0, "test_mismatch");
        let result = store.store_event(event_mismatch).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            InMemoryEventStoreBackendError::VersionMismatch(event_version, stream_version) => {
                assert_eq!(event_version, 0);
                assert_eq!(stream_version, 1);
            }
            _ => panic!("Unexpected error type"),
        }
    }

    #[tokio::test]
    async fn in_memory_event_store_read_events() {
        let bus = InMemoryEventBus::<MyEventData>::new();
        let store = InMemoryEventStore::new(bus);
        let stream_id = Uuid::new_v4();

        let event1 = new_event(stream_id, 0, "test1");
        let event2 = new_event(stream_id, 1, "test2");
        let event3 = new_event(stream_id, 2, "test3");

        store.store_event(event1.clone()).await.unwrap();
        store.store_event(event2.clone()).await.unwrap();
        store.store_event(event3.clone()).await.unwrap();

        let mut stream = store.read_events(stream_id).await.unwrap();

        assert_eq!(stream.next().await.unwrap().unwrap(), event1);
        assert_eq!(stream.next().await.unwrap().unwrap(), event2);
        assert_eq!(stream.next().await.unwrap().unwrap(), event3);
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn in_memory_event_bus_new() {
        let bus = InMemoryEventBus::<MyEventData>::new();
        assert!(bus.projections.lock().await.is_empty());
    }

    #[tokio::test]
    async fn in_memory_event_bus_publish() {
        let bus = InMemoryEventBus::<MyEventData>::new();
        let projection = TestProjection::new();
        let storage = projection.get_state_store().clone();
        bus.subscribe(projection).await.unwrap();

        let stream_id = Uuid::new_v4();
        let event = new_event(stream_id, 0, "test_publish");
        bus.publish(event.clone()).await.unwrap();

        let events: TestState = storage.get_state(stream_id).await.unwrap().unwrap();
        assert_eq!(events.0.len(), 1);
        assert_eq!(events.0[0], event);
    }

    #[tokio::test]
    async fn in_memory_event_bus_subscribe() {
        let bus = InMemoryEventBus::<MyEventData>::new();

        let projection = TestProjection::new();

        bus.subscribe(projection).await.unwrap();

        let projections = bus.projections.lock().await;
        assert_eq!(projections.len(), 1);
    }
}
