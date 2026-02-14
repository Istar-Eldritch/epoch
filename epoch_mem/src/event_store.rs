use async_trait::async_trait;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

use futures_core::Stream;

use epoch_core::prelude::*;

/// The in-memory data store.
#[derive(Debug)]
struct EventStoreData<D: EventData> {
    events: HashMap<Uuid, Arc<Event<D>>>,
    stream_events: HashMap<Uuid, Vec<Uuid>>,
    stream_version: HashMap<Uuid, u64>,
    correlation_events: HashMap<Uuid, Vec<Uuid>>,
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
                correlation_events: HashMap::new(),
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
    /// Event version doesn't match the stream version
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
    ) -> Result<Pin<Box<dyn EventStream<Self::EventType, Self::Error> + Send + 'life0>>, Self::Error>
    {
        self.read_events_since(stream_id, 1).await
    }

    /// Fetches a stream from the storage backend.
    async fn read_events_since(
        &self,
        stream_id: Uuid,
        version: u64,
    ) -> Result<Pin<Box<dyn EventStream<Self::EventType, Self::Error> + Send + 'life0>>, Self::Error>
    {
        log::debug!(
            "Reading events for stream_id: {} since version: {}",
            stream_id,
            version
        );
        let data = self.data.clone();
        let stream: Pin<Box<dyn EventStream<Self::EventType, Self::Error> + Send>> = Box::pin(
            InMemoryEventStoreStream::<B, Self::Error>::new(data, stream_id, version - 1),
        );
        Ok(stream)
    }

    async fn store_event(&self, event: Event<Self::EventType>) -> Result<(), Self::Error> {
        let data = self.data.clone();
        let bus = self.bus.clone();
        let mut data = data.lock().await;
        let stream_id = event.stream_id;

        let version: u64 = *data.stream_version.get(&stream_id).unwrap_or(&1);

        log::debug!(
            "store_event: stream_id: {}, event.stream_version: {}, current_store_version: {}",
            stream_id,
            event.stream_version,
            version
        );
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
        // Maintain correlation index
        if let Some(correlation_id) = event.correlation_id {
            data.correlation_events
                .entry(correlation_id)
                .or_default()
                .push(event_id);
        }
        // Wrap in Arc once for efficient sharing between storage and bus
        let event = Arc::new(event);
        data.events.insert(event_id, Arc::clone(&event));
        data.stream_events
            .entry(stream_id)
            .or_default()
            .extend(&[event_id]);
        log::debug!(
            "Event stored successfully for stream_id: {}, event_id: {}",
            stream_id,
            event_id
        );
        if bus.publish(event).await.is_err() {
            return Err(InMemoryEventStoreBackendError::PublishEvent);
        }
        Ok(())
    }

    async fn store_events(&self, events: Vec<Event<Self::EventType>>) -> Result<(), Self::Error> {
        if events.is_empty() {
            return Ok(());
        }

        let data = self.data.clone();
        let bus = self.bus.clone();
        let mut data = data.lock().await;

        // Phase 1: Validate all versions first (atomic check)
        // Group events by stream_id to validate versions correctly
        let mut stream_versions: std::collections::HashMap<Uuid, u64> =
            std::collections::HashMap::new();
        for event in &events {
            let expected_version = stream_versions
                .get(&event.stream_id)
                .copied()
                .unwrap_or_else(|| *data.stream_version.get(&event.stream_id).unwrap_or(&1));

            if event.stream_version != expected_version {
                log::debug!(
                    "Event version mismatch for stream_id: {}. Expected: {}, Got: {}",
                    event.stream_id,
                    expected_version,
                    event.stream_version
                );
                return Err(InMemoryEventStoreBackendError::VersionMismatch(
                    event.stream_version,
                    expected_version,
                ));
            }

            // Track what the next version should be for this stream
            stream_versions.insert(event.stream_id, expected_version + 1);
        }

        // Phase 2: Store all events (all validations passed)
        let mut stored_events = Vec::with_capacity(events.len());
        for event in events {
            let stream_id = event.stream_id;
            let new_version = event.stream_version + 1;

            data.stream_version.insert(stream_id, new_version);

            let event_id = event.id;
            // Maintain correlation index
            if let Some(correlation_id) = event.correlation_id {
                data.correlation_events
                    .entry(correlation_id)
                    .or_default()
                    .push(event_id);
            }
            let event = Arc::new(event);
            data.events.insert(event_id, Arc::clone(&event));
            data.stream_events
                .entry(stream_id)
                .or_default()
                .push(event_id);

            stored_events.push(event);
        }

        // Release lock before publishing
        drop(data);

        // Phase 3: Publish all events
        // Note: If publishing fails partway through, events are stored but not all published.
        // This is acceptable for event sourcing - events are durable and projections can
        // catch up by replaying from the event store.
        for event in stored_events {
            if bus.publish(event).await.is_err() {
                return Err(InMemoryEventStoreBackendError::PublishEvent);
            }
        }

        Ok(())
    }

    async fn read_events_by_correlation_id(
        &self,
        correlation_id: Uuid,
    ) -> Result<Vec<Event<Self::EventType>>, Self::Error> {
        let data = self.data.lock().await;
        let event_ids = match data.correlation_events.get(&correlation_id) {
            Some(ids) => ids.clone(),
            None => return Ok(vec![]),
        };
        let events: Vec<Event<Self::EventType>> = event_ids
            .iter()
            .filter_map(|id| data.events.get(id).map(|e| (**e).clone()))
            .collect();
        Ok(events)
    }

    async fn trace_causation_chain(
        &self,
        event_id: Uuid,
    ) -> Result<Vec<Event<Self::EventType>>, Self::Error> {
        let data = self.data.lock().await;

        // Look up the event
        let event = match data.events.get(&event_id) {
            Some(e) => e.clone(),
            None => return Ok(vec![]),
        };

        // If no correlation ID, just return this single event
        let correlation_id = match event.correlation_id {
            Some(cid) => cid,
            None => return Ok(vec![(*event).clone()]),
        };

        // Get all correlated events
        let event_ids = match data.correlation_events.get(&correlation_id) {
            Some(ids) => ids.clone(),
            None => return Ok(vec![(*event).clone()]),
        };
        let correlated_events: Vec<Event<Self::EventType>> = event_ids
            .iter()
            .filter_map(|id| data.events.get(id).map(|e| (**e).clone()))
            .collect();

        // Release lock before calling the pure function
        drop(data);

        Ok(epoch_core::causation::extract_causation_subtree(
            correlated_events,
            event_id,
        ))
    }
}

/// An in-memory event store stream.
pub struct InMemoryEventStoreStream<B, E>
where
    B: EventBus + Clone,
{
    id: Uuid,
    data: Arc<Mutex<EventStoreData<B::EventType>>>,
    current_index: usize,
    _phantom: PhantomData<(B, E)>,
}

impl<B, E> InMemoryEventStoreStream<B, E>
where
    B: EventBus + Clone,
    E: std::error::Error + Send + Sync,
{
    fn new(data: Arc<Mutex<EventStoreData<B::EventType>>>, id: Uuid, start_version: u64) -> Self {
        Self {
            data,
            id,
            current_index: start_version as usize,
            _phantom: PhantomData,
        }
    }
}

// All fields (Uuid, Arc, usize, PhantomData) are Unpin, so InMemoryEventStoreStream is Unpin.
impl<B, E> Unpin for InMemoryEventStoreStream<B, E> where B: EventBus + Clone {}

impl<B, E> EventStream<B::EventType, E> for InMemoryEventStoreStream<B, E>
where
    B: EventBus + Clone + Send + Sync,
    B::EventType: Clone,
    E: std::error::Error + Send + Sync,
{
}

impl<B, E> Stream for InMemoryEventStoreStream<B, E>
where
    B: EventBus + Clone + Send + Sync,
    B::EventType: Send + Sync + Clone,
    E: std::error::Error + Send + Sync,
{
    type Item = Result<Event<B::EventType>, E>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();

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
                event_ids.len() - this.current_index,
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
                    // Dereference Arc and clone the inner Event for the stream
                    // This is necessary because EventStream yields owned Event<D>, not Arc<Event<D>>
                    return Poll::Ready(Some(Ok((**event).clone())));
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
///
/// Uses a channel-based architecture similar to `PgEventBus`:
/// - `publish()` sends events to a channel and returns immediately
/// - A background task processes events and notifies observers
/// - This matches production PostgreSQL NOTIFY/LISTEN behavior
///
/// This design prevents synchronous call chains that can cause
/// race conditions in sagas and projections.
#[derive(Clone)]
pub struct InMemoryEventBus<D>
where
    D: EventData + Send + Sync + 'static,
{
    _phantom: PhantomData<D>,
    projections: Arc<RwLock<Vec<Box<dyn EventObserver<D>>>>>,
    event_tx: tokio::sync::mpsc::UnboundedSender<Arc<Event<D>>>,
}

impl<D> Default for InMemoryEventBus<D>
where
    D: EventData + Send + Sync,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<D> InMemoryEventBus<D>
where
    D: EventData + Send + Sync,
{
    /// Creates a new in-memory bus
    pub fn new() -> Self {
        log::debug!("Creating a new InMemoryEventBus");
        let projections: Arc<RwLock<Vec<Box<dyn EventObserver<D>>>>> =
            Arc::new(RwLock::new(vec![]));

        // Create channel for event notifications (like PostgreSQL NOTIFY)
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<Arc<Event<D>>>();

        // Spawn background task to process events (like PostgreSQL LISTEN)
        let projections_clone = projections.clone();
        tokio::spawn(async move {
            log::debug!("InMemoryEventBus: Starting background event processor");

            while let Some(event) = event_rx.recv().await {
                log::debug!("InMemoryEventBus: Processing event {}", event.id);

                // Process observers sequentially (maintaining order within this task)
                let observers = projections_clone.read().await;
                for observer in observers.iter() {
                    if let Err(e) = observer.on_event(Arc::clone(&event)).await {
                        log::error!("Error applying event {}: {:?}", event.id, e);
                        //TODO: Retry mechanism and dead letter queue
                    }
                }
            }

            log::debug!("InMemoryEventBus: Background event processor stopped");
        });

        Self {
            _phantom: PhantomData,
            projections,
            event_tx,
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
pub enum InMemoryEventBusError<D>
where
    D: EventData + std::fmt::Debug,
{
    /// The event could not be published
    #[error("Error publishing event: {0}")]
    PublishError(#[from] tokio::sync::mpsc::error::SendError<Arc<Event<D>>>),
}

impl<D> EventBus for InMemoryEventBus<D>
where
    D: EventData + Send + Sync + std::fmt::Debug + 'static,
{
    type Error = InMemoryEventBusError<D>;
    type EventType = D;
    fn publish<'a>(
        &'a self,
        event: Arc<Event<Self::EventType>>,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async move {
            log::debug!("Publishing event with id: {}", event.id);

            // Send to channel and return immediately (like PgEventBus NO-OP + NOTIFY)
            self.event_tx
                .send(event)
                .map_err(|e| InMemoryEventBusError::PublishError(e))?;

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
            let mut projectors = projectors.write().await;
            projectors.push(Box::new(projector));
            Ok(())
        })
    }
}

/// Implementation of an InMemory State Storage
#[derive(Clone, Debug)]
pub struct InMemoryStateStore<T>(std::sync::Arc<Mutex<HashMap<Uuid, T>>>)
where
    T: Clone + std::fmt::Debug;

impl<T> Default for InMemoryStateStore<T>
where
    T: Clone + std::fmt::Debug,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<T> InMemoryStateStore<T>
where
    T: Clone + std::fmt::Debug,
{
    /// Creates a new InMemoryStateStore
    pub fn new() -> Self {
        InMemoryStateStore(Arc::new(Mutex::new(HashMap::new())))
    }
}

/// Error that may be raised by the InMemoryStateStore
#[derive(Debug, thiserror::Error)]
pub enum InMemoryStateStoreError {}

#[async_trait]
impl<T> StateStoreBackend<T> for InMemoryStateStore<T>
where
    T: Clone + Send + Sync + std::fmt::Debug,
{
    type Error = InMemoryStateStoreError;
    async fn get_state(&self, id: Uuid) -> Result<Option<T>, Self::Error> {
        let data = self.0.lock().await;
        Ok(data.get(&id).cloned())
    }
    async fn persist_state(&mut self, id: Uuid, state: T) -> Result<(), Self::Error> {
        let mut data = self.0.lock().await;
        data.insert(id, state);
        Ok(())
    }
    async fn delete_state(&mut self, id: Uuid) -> Result<(), Self::Error> {
        let mut data = self.0.lock().await;
        data.remove(&id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use epoch_core::event::EventData;
    use tokio_stream::StreamExt;
    use uuid::Uuid;

    use epoch_core::event::EnumConversionError;

    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    struct MyEventData {
        value: String,
    }

    impl EventData for MyEventData {
        fn event_type(&self) -> &'static str {
            "MyEvent"
        }
    }

    // Identity conversion for testing - clones the data
    impl TryFrom<&MyEventData> for MyEventData {
        type Error = EnumConversionError;

        fn try_from(value: &MyEventData) -> Result<Self, Self::Error> {
            Ok(value.clone())
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

    impl epoch_core::SubscriberId for TestProjection {
        fn subscriber_id(&self) -> &str {
            "projection:test"
        }
    }

    #[derive(Debug, Clone)]
    struct TestState(Vec<Event<MyEventData>>);

    impl EventApplicatorState for TestState {
        fn get_id(&self) -> &Uuid {
            // For testing purposes, return a static UUID reference
            static TEST_UUID: std::sync::OnceLock<Uuid> = std::sync::OnceLock::new();
            TEST_UUID.get_or_init(Uuid::new_v4)
        }
    }

    #[derive(Debug, thiserror::Error)]
    pub enum TestProjectionError {}

    impl EventApplicator<MyEventData> for TestProjection {
        type State = TestState;
        type StateStore = InMemoryStateStore<Self::State>;
        type EventType = MyEventData;
        type ApplyError = TestProjectionError;

        fn get_state_store(&self) -> Self::StateStore {
            self.0.clone()
        }
        fn apply(
            &self,
            state: Option<Self::State>,
            event: &Event<Self::EventType>,
        ) -> Result<Option<Self::State>, Self::ApplyError> {
            if let Some(mut state) = state {
                state.0.push(event.clone());
                Ok(Some(state))
            } else {
                Ok(Some(TestState(vec![event.clone()])))
            }
        }
    }

    impl Projection<MyEventData> for TestProjection {}

    #[tokio::test]
    async fn in_memory_event_store_new() {
        let bus = InMemoryEventBus::<MyEventData>::new();
        let store = InMemoryEventStore::new(bus.clone());
        assert!(store.data.lock().await.events.is_empty());
        assert!(store.data.lock().await.stream_events.is_empty());
        assert!(store.data.lock().await.stream_version.is_empty());
        assert!(store.bus().projections.read().await.is_empty());
    }

    #[tokio::test]
    async fn in_memory_event_store_store_event() {
        let bus = InMemoryEventBus::<MyEventData>::new();
        let store = InMemoryEventStore::new(bus);
        let stream_id = Uuid::new_v4();

        let event1 = new_event(stream_id, 1, "test1");
        store.store_event(event1.clone()).await.unwrap();

        let event2 = new_event(stream_id, 2, "test2");
        store.store_event(event2.clone()).await.unwrap();

        let data = store.data.lock().await;
        assert_eq!(data.events.len(), 2);
        assert_eq!(data.stream_events.get(&stream_id).unwrap().len(), 2);
        assert_eq!(*data.stream_version.get(&stream_id).unwrap(), 3);
    }

    #[tokio::test]
    async fn in_memory_event_store_store_event_version_mismatch() {
        let bus = InMemoryEventBus::<MyEventData>::new();
        let store = InMemoryEventStore::new(bus);
        let stream_id = Uuid::new_v4();

        let event1 = new_event(stream_id, 1, "test1");
        store.store_event(event1.clone()).await.unwrap();

        let event_mismatch = new_event(stream_id, 1, "test_mismatch");
        let result = store.store_event(event_mismatch).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            InMemoryEventStoreBackendError::VersionMismatch(event_version, stream_version) => {
                assert_eq!(event_version, 1);
                assert_eq!(stream_version, 2);
            }
            _ => panic!("Unexpected error type"),
        }
    }

    #[tokio::test]
    async fn in_memory_event_store_read_events() {
        let bus = InMemoryEventBus::<MyEventData>::new();
        let store = InMemoryEventStore::new(bus);
        let stream_id = Uuid::new_v4();

        let event1 = new_event(stream_id, 1, "test1");
        let event2 = new_event(stream_id, 2, "test2");
        let event3 = new_event(stream_id, 3, "test3");

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
    async fn in_memory_event_store_read_events_since() {
        let bus = InMemoryEventBus::<MyEventData>::new();
        let store = InMemoryEventStore::new(bus);
        let stream_id = Uuid::new_v4();

        let event1 = new_event(stream_id, 1, "test1");
        let event2 = new_event(stream_id, 2, "test2");
        let event3 = new_event(stream_id, 3, "test3");

        store.store_event(event1.clone()).await.unwrap();
        store.store_event(event2.clone()).await.unwrap();
        store.store_event(event3.clone()).await.unwrap();

        let mut stream = store.read_events_since(stream_id, 2).await.unwrap(); // Starts from index 1 (event2)

        assert_eq!(stream.next().await.unwrap().unwrap(), event2);
        assert_eq!(stream.next().await.unwrap().unwrap(), event3);
        assert!(stream.next().await.is_none());

        let mut stream_from_version_3 = store.read_events_since(stream_id, 3).await.unwrap(); // Starts from index 2 (event3)
        assert_eq!(stream_from_version_3.next().await.unwrap().unwrap(), event3);
        assert!(stream_from_version_3.next().await.is_none());

        let mut stream_from_version_4 = store.read_events_since(stream_id, 4).await.unwrap(); // Starts from index 3 (non-existent)
        assert!(stream_from_version_4.next().await.is_none());
    }

    #[tokio::test]
    async fn in_memory_event_bus_new() {
        let bus = InMemoryEventBus::<MyEventData>::new();
        assert!(bus.projections.read().await.is_empty());
    }

    #[tokio::test]
    async fn in_memory_event_bus_publish() {
        let bus = InMemoryEventBus::<MyEventData>::new();
        let projection = TestProjection::new();
        let storage = projection.get_state_store().clone();
        bus.subscribe(ProjectionHandler::new(projection))
            .await
            .unwrap();

        let stream_id = Uuid::new_v4();
        let event = new_event(stream_id, 1, "test_publish");
        bus.publish(Arc::new(event.clone())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let events: TestState = storage.get_state(stream_id).await.unwrap().unwrap();
        assert_eq!(events.0.len(), 1);
        assert_eq!(events.0[0], event);
    }

    #[tokio::test]
    async fn in_memory_event_bus_subscribe() {
        let bus = InMemoryEventBus::<MyEventData>::new();

        let projection = TestProjection::new();

        bus.subscribe(ProjectionHandler::new(projection))
            .await
            .unwrap();

        let projections = bus.projections.read().await;
        assert_eq!(projections.len(), 1);
    }

    #[tokio::test]
    async fn in_memory_event_store_store_events_batch() {
        let bus = InMemoryEventBus::<MyEventData>::new();
        let store = InMemoryEventStore::new(bus);
        let stream_id = Uuid::new_v4();

        let events: Vec<Event<MyEventData>> = (1..=5)
            .map(|i| new_event(stream_id, i, &format!("test{}", i)))
            .collect();

        store.store_events(events).await.unwrap();

        let data = store.data.lock().await;
        assert_eq!(data.events.len(), 5);
        assert_eq!(data.stream_events.get(&stream_id).unwrap().len(), 5);
        assert_eq!(*data.stream_version.get(&stream_id).unwrap(), 6);
    }

    #[tokio::test]
    async fn in_memory_event_store_store_events_empty_batch() {
        let bus = InMemoryEventBus::<MyEventData>::new();
        let store = InMemoryEventStore::new(bus);

        // Empty batch should succeed
        store.store_events(Vec::new()).await.unwrap();

        let data = store.data.lock().await;
        assert!(data.events.is_empty());
    }

    #[tokio::test]
    async fn in_memory_event_store_store_events_version_mismatch_aborts_all() {
        let bus = InMemoryEventBus::<MyEventData>::new();
        let store = InMemoryEventStore::new(bus);
        let stream_id = Uuid::new_v4();

        // First, store one event normally
        let event1 = new_event(stream_id, 1, "test1");
        store.store_event(event1).await.unwrap();

        // Try to store batch with version mismatch in the middle
        let events = vec![
            new_event(stream_id, 2, "test2"), // OK
            new_event(stream_id, 3, "test3"), // OK
            new_event(stream_id, 5, "test5"), // Version mismatch! (should be 4)
        ];

        let result = store.store_events(events).await;
        assert!(result.is_err());

        // Verify NO events from the batch were stored (atomic)
        let data = store.data.lock().await;
        assert_eq!(data.events.len(), 1); // Only the original event
        assert_eq!(data.stream_events.get(&stream_id).unwrap().len(), 1);
        assert_eq!(*data.stream_version.get(&stream_id).unwrap(), 2);
    }

    #[tokio::test]
    async fn in_memory_event_store_store_events_multiple_streams() {
        let bus = InMemoryEventBus::<MyEventData>::new();
        let store = InMemoryEventStore::new(bus);
        let stream_id_a = Uuid::new_v4();
        let stream_id_b = Uuid::new_v4();

        // Events for two different streams in the same batch
        let events = vec![
            new_event(stream_id_a, 1, "a1"),
            new_event(stream_id_b, 1, "b1"),
            new_event(stream_id_a, 2, "a2"),
            new_event(stream_id_b, 2, "b2"),
        ];

        store.store_events(events).await.unwrap();

        let data = store.data.lock().await;
        assert_eq!(data.events.len(), 4);
        assert_eq!(data.stream_events.get(&stream_id_a).unwrap().len(), 2);
        assert_eq!(data.stream_events.get(&stream_id_b).unwrap().len(), 2);
        assert_eq!(*data.stream_version.get(&stream_id_a).unwrap(), 3);
        assert_eq!(*data.stream_version.get(&stream_id_b).unwrap(), 3);
    }

    #[tokio::test]
    async fn store_and_read_event_with_causation() {
        let bus = InMemoryEventBus::<MyEventData>::default();
        let store = InMemoryEventStore::new(bus);

        let correlation_id = Uuid::new_v4();
        let causation_id = Uuid::new_v4();
        let stream_id = Uuid::new_v4();

        let event = Event::<MyEventData>::builder()
            .stream_id(stream_id)
            .stream_version(1)
            .event_type("Created".to_string())
            .data(Some(MyEventData {
                value: "test".to_string(),
            }))
            .correlation_id(correlation_id)
            .causation_id(causation_id)
            .build()
            .unwrap();

        store.store_event(event.clone()).await.unwrap();

        // Read back and verify causation fields preserved
        let mut stream = store.read_events(stream_id).await.unwrap();
        let retrieved = stream.next().await.unwrap().unwrap();

        assert_eq!(retrieved.correlation_id, Some(correlation_id));
        assert_eq!(retrieved.causation_id, Some(causation_id));
    }

    /// Helper to create an event with causation/correlation metadata.
    fn new_correlated_event(
        stream_id: Uuid,
        stream_version: u64,
        value: &str,
        correlation_id: Uuid,
        causation_id: Option<Uuid>,
    ) -> Event<MyEventData> {
        let mut builder = Event::<MyEventData>::builder()
            .stream_id(stream_id)
            .event_type("MyEvent".to_string())
            .stream_version(stream_version)
            .data(Some(MyEventData {
                value: value.to_string(),
            }))
            .correlation_id(correlation_id);
        if let Some(cid) = causation_id {
            builder = builder.causation_id(cid);
        }
        builder.build().unwrap()
    }

    #[tokio::test]
    async fn read_events_by_correlation_id_returns_matching_events() {
        let bus = InMemoryEventBus::<MyEventData>::new();
        let store = InMemoryEventStore::new(bus);

        let corr_id = Uuid::new_v4();
        let stream_a = Uuid::new_v4();
        let stream_b = Uuid::new_v4();

        let event1 = new_correlated_event(stream_a, 1, "a1", corr_id, None);
        let event2 = new_correlated_event(stream_b, 1, "b1", corr_id, Some(event1.id));

        store.store_event(event1.clone()).await.unwrap();
        store.store_event(event2.clone()).await.unwrap();

        let events = store.read_events_by_correlation_id(corr_id).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].id, event1.id);
        assert_eq!(events[1].id, event2.id);
    }

    #[tokio::test]
    async fn read_events_by_correlation_id_returns_empty_for_nonexistent() {
        let bus = InMemoryEventBus::<MyEventData>::new();
        let store = InMemoryEventStore::new(bus);

        let events = store
            .read_events_by_correlation_id(Uuid::new_v4())
            .await
            .unwrap();
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn read_events_by_correlation_id_excludes_unrelated() {
        let bus = InMemoryEventBus::<MyEventData>::new();
        let store = InMemoryEventStore::new(bus);

        let corr_a = Uuid::new_v4();
        let corr_b = Uuid::new_v4();
        let stream = Uuid::new_v4();

        let event1 = new_correlated_event(stream, 1, "a", corr_a, None);
        let event2 = new_correlated_event(stream, 2, "b", corr_b, None);

        store.store_event(event1.clone()).await.unwrap();
        store.store_event(event2.clone()).await.unwrap();

        let events = store.read_events_by_correlation_id(corr_a).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, event1.id);
    }

    #[tokio::test]
    async fn read_events_by_correlation_id_via_store_events_batch() {
        let bus = InMemoryEventBus::<MyEventData>::new();
        let store = InMemoryEventStore::new(bus);

        let corr_id = Uuid::new_v4();
        let stream = Uuid::new_v4();

        let event1 = new_correlated_event(stream, 1, "a", corr_id, None);
        let event2 = new_correlated_event(stream, 2, "b", corr_id, Some(event1.id));

        store
            .store_events(vec![event1.clone(), event2.clone()])
            .await
            .unwrap();

        let events = store.read_events_by_correlation_id(corr_id).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].id, event1.id);
        assert_eq!(events[1].id, event2.id);
    }

    #[tokio::test]
    async fn trace_causation_chain_linear() {
        let bus = InMemoryEventBus::<MyEventData>::new();
        let store = InMemoryEventStore::new(bus);

        let corr_id = Uuid::new_v4();
        let stream_a = Uuid::new_v4();
        let stream_b = Uuid::new_v4();
        let stream_c = Uuid::new_v4();

        // A → B → C chain across 3 streams
        let event_a = new_correlated_event(stream_a, 1, "a", corr_id, None);
        let event_b = new_correlated_event(stream_b, 1, "b", corr_id, Some(event_a.id));
        let event_c = new_correlated_event(stream_c, 1, "c", corr_id, Some(event_b.id));

        store.store_event(event_a.clone()).await.unwrap();
        store.store_event(event_b.clone()).await.unwrap();
        store.store_event(event_c.clone()).await.unwrap();

        // Trace from the middle event
        let chain = store.trace_causation_chain(event_b.id).await.unwrap();
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0].id, event_a.id);
        assert_eq!(chain[1].id, event_b.id);
        assert_eq!(chain[2].id, event_c.id);
    }

    #[tokio::test]
    async fn trace_causation_chain_not_found() {
        let bus = InMemoryEventBus::<MyEventData>::new();
        let store = InMemoryEventStore::new(bus);

        let chain = store.trace_causation_chain(Uuid::new_v4()).await.unwrap();
        assert!(chain.is_empty());
    }

    #[tokio::test]
    async fn trace_causation_chain_no_correlation() {
        let bus = InMemoryEventBus::<MyEventData>::new();
        let store = InMemoryEventStore::new(bus);
        let stream = Uuid::new_v4();

        // Event without correlation_id
        let event = new_event(stream, 1, "lonely");
        store.store_event(event.clone()).await.unwrap();

        let chain = store.trace_causation_chain(event.id).await.unwrap();
        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].id, event.id);
    }

    #[tokio::test]
    async fn trace_causation_chain_excludes_sibling_branch() {
        let bus = InMemoryEventBus::<MyEventData>::new();
        let store = InMemoryEventStore::new(bus);

        let corr_id = Uuid::new_v4();
        let stream_a = Uuid::new_v4();
        let stream_b = Uuid::new_v4();
        let stream_c = Uuid::new_v4();

        // A → B, A → C (branching), trace from B should exclude C
        let event_a = new_correlated_event(stream_a, 1, "a", corr_id, None);
        let event_b = new_correlated_event(stream_b, 1, "b", corr_id, Some(event_a.id));
        let event_c = new_correlated_event(stream_c, 1, "c", corr_id, Some(event_a.id));

        store.store_event(event_a.clone()).await.unwrap();
        store.store_event(event_b.clone()).await.unwrap();
        store.store_event(event_c.clone()).await.unwrap();

        let chain = store.trace_causation_chain(event_b.id).await.unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].id, event_a.id);
        assert_eq!(chain[1].id, event_b.id);
    }
}
