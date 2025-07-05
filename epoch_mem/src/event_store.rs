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
struct EventStoreData<D: EventData> {
    events: HashMap<Uuid, Event<D>>,
    stream_events: HashMap<Uuid, Vec<Uuid>>,
    sequence_number: u64,
}

/// An in-memory event store.
///
/// This event store is useful for testing and development purposes. It is not recommended for
/// production use, as it does not persist events to any durable storage.
#[derive(Clone)]
pub struct MemEventStore<B: EventBus> {
    data: Arc<Mutex<EventStoreData<B::EventType>>>,
    bus: B,
}

impl<B: EventBus> MemEventStore<B> {
    /// Creates a new `MemEventStore`.
    pub fn new(bus: B) -> Self {
        Self {
            data: Arc::new(Mutex::new(EventStoreData {
                events: HashMap::new(),
                stream_events: HashMap::new(),
                sequence_number: 0,
            })),
            bus,
        }
    }

    /// Exposes the event store bus
    pub fn bus(&self) -> &B {
        &self.bus
    }
}

/// An error that can occur when fetching a stream.
#[derive(Debug, thiserror::Error)]
pub enum EventStreamFetchError {
    /// An unexpected error occurred.
    #[error("unexpected error: {0}")]
    Unexpected(#[from] Box<dyn std::error::Error>),
}

#[async_trait::async_trait]
impl<B> EventStoreBackend for MemEventStore<B>
where
    B: EventBus + Send + Sync,
    B::EventType: EventData + Send + Sync,
{
    type EventType = B::EventType;
    type AppendToStreamError = EventStreamAppendError;
    async fn read_events<'a, D: EventData + Send + Sync + TryFrom<Self::EventType>>(
        &'a self,
        stream_id: Uuid,
    ) -> Result<MemEventStoreStream<'a, B, D>, EventStreamFetchError>
    where
        B::EventType: From<D>,
    {
        Ok(MemEventStoreStream::<'_, B, D>::new(self, stream_id))
    }

    async fn store_event<D>(
        &self,
        event: Event<D>,
    ) -> Result<Event<Self::EventType>, Self::AppendToStreamError>
    where
        D: EventData + Send + Sync,
        Self::EventType: From<D>,
    {
        let mut data = self.data.lock().await;
        let event_data = event.data.clone();
        data.sequence_number += 1;

        let event: Event<Self::EventType> = event
            .into_builder()
            .data(event_data.map(|d| d.into()))
            .sequence_number(data.sequence_number)
            .build()
            .expect("To build event from existing one");
        let stream_id = event.stream_id;
        let event_id = event.id;
        data.events.insert(event_id, event.clone());
        data.stream_events
            .entry(stream_id)
            .or_insert_with(Vec::new)
            .extend(&[event_id]);
        Ok(event)
    }
}

/// An error that can occur when appending to a stream.
#[derive(Debug, thiserror::Error)]
pub enum EventStreamAppendError {
    /// An unexpected error occurred.
    #[error("unexpected error: {0}")]
    Unexpected(#[from] Box<dyn std::error::Error>),
}

/// An in--memory event store stream.
pub struct MemEventStoreStream<'a, B, D>
where
    B: EventBus,
    B::EventType: EventData + From<D>,
    D: EventData + Send + Sync + TryFrom<B::EventType>,
{
    store: &'a MemEventStore<B>,
    _phantom: PhantomData<D>,
    id: Uuid,
    current_index: usize,
}

#[async_trait::async_trait]
impl<'a, B, D> EventStream<D> for MemEventStoreStream<'a, B, D>
where
    B: EventBus,
    B::EventType: EventData + Send + Sync + From<D>,
    D: EventData + Send + Sync + TryFrom<B::EventType>,
{
}

impl<'a, B, D> MemEventStoreStream<'a, B, D>
where
    B: EventBus,
    B::EventType: EventData + From<D>,
    D: EventData + Send + Sync + TryFrom<B::EventType>,
{
    fn new(store: &'a MemEventStore<B>, id: Uuid) -> Self {
        Self {
            store,
            id,
            _phantom: PhantomData,
            current_index: 0,
        }
    }
}

impl<'a, B, D> Stream for MemEventStoreStream<'a, B, D>
where
    B: EventBus,
    B::EventType: EventData + Send + Sync + From<D>,
    D: EventData + Send + Sync + TryFrom<B::EventType>,
{
    type Item = Event<D>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // We need to use unsafe to get a mutable reference to the fields of the `!Unpin` struct.
        // This is safe because we are not moving the `VirtualEventStoreStream` itself.
        let this = unsafe { self.get_unchecked_mut() };

        let data = match this.store.data.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
        };

        if let Some(event_ids) = data.stream_events.get(&this.id) {
            while this.current_index < event_ids.len() {
                let event_id = event_ids[this.current_index];
                this.current_index += 1;

                // Find the actual event in the store's main events vector
                if let Some(event) = data.events.get(&event_id) {
                    // Convert event P to D if possible
                    if let Some(data_p) = &event.data {
                        if let Ok(data_d) = D::try_from(data_p.clone()) {
                            let converted_event = event
                                .clone()
                                .into_builder()
                                .data(Some(data_d))
                                .build()
                                .expect("Event to be buildable");

                            return Poll::Ready(Some(converted_event));
                        }
                    }
                }
            }
        }
        Poll::Ready(None)
    }
}

/// The error for the InMemoryEventBus event publication
#[derive(Debug, thiserror::Error)]
#[error("In memory eventbus publish error")]
pub struct InMemoryEventBusPublishError;

/// An implementation of an in-memory event bus
#[derive(Debug)]
pub struct InMemoryEventBus<D, P>
where
    D: EventData + Send + Sync,
    P: Projector + Send + Sync,
    <P::Projection as Projection>::EventType: From<D> + EventData,
{
    _phantom: PhantomData<D>,
    projectors: Arc<Mutex<Vec<P>>>,
}

impl<D, P> EventBus for InMemoryEventBus<D, P>
where
    D: EventData + Send + Sync,
    P: Projector + Send + Sync,
    <P::Projection as Projection>::EventType: From<D> + EventData,
{
    type EventType = D;
    type PublishError = InMemoryEventBusPublishError;
    type ProjectorType = P;

    fn subscribe<'a>(&'a self, projector: Self::ProjectorType) -> impl Future<Output = ()> + Send {
        async {
            let mut projectors = self.projectors.lock().await;
            projectors.push(projector);
        }
    }

    fn publish<'a, E>(
        &'a self,
        event: Event<E>,
    ) -> impl Future<Output = Result<(), Self::PublishError>> + Send
    where
        E: EventData + 'a + Send + Sync + Into<D>,
    {
        async move {
            let projectors = self.projectors.lock().await;
            for projector in projectors.iter() {
                let event_for_projection = event
                    .clone()
                    .into_builder()
                    .data(event.data.clone().map(|d| d.into().into()))
                    .build()
                    .unwrap();
                projector.project(event_for_projection).await.unwrap();
            }
            Ok(())
        }
    }
}
