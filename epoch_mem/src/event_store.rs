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
    sequence_number: u64,
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

/// Errors returned by the InMemoryEventBus
#[derive(Debug, thiserror::Error)]
pub enum InMemoryEventStoreBackendError {
    /// Error publishing event to bus
    #[error("Error publishing event to bus")]
    PublishEvent,
}

impl<B> EventStoreBackend for InMemoryEventStore<B>
where
    B: EventBus + Send + Sync + Clone,
    B::Error: 'static,
{
    type Error = InMemoryEventStoreBackendError;
    type EventType = B::EventType;
    #[allow(refining_impl_trait)]
    fn read_events<D>(
        &self,
        stream_id: Uuid,
    ) -> impl Future<Output = Result<InMemoryEventStoreStream<B, D>, Self::Error>> + Send
    where
        D: TryFrom<Self::EventType> + EventData + Send + Sync,
        Self::EventType: From<D>,
    {
        let store = self.clone();
        async move { Ok(InMemoryEventStoreStream::<B, D>::new(store, stream_id)) }
    }

    fn store_event<D>(
        &self,
        event: Event<D>,
    ) -> impl Future<Output = Result<Event<Self::EventType>, Self::Error>> + Send
    where
        D: TryFrom<Self::EventType> + EventData + Send + Sync,
        Self::EventType: From<D>,
    {
        async move {
            let mut data = self.data.lock().await;
            let event_data = event.data.clone();
            data.sequence_number += 1;

            let event: Event<Self::EventType> = event
                .into_builder()
                .data(event_data.map(|d| d.try_into().unwrap()))
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
            self.bus()
                .publish(event.clone())
                .await
                // TODO: Deal with this error acordingly
                .map_err(|_e| InMemoryEventStoreBackendError::PublishEvent)?;
            Ok(event)
        }
    }
}

/// An in--memory event store stream.
pub struct InMemoryEventStoreStream<B, D>
where
    B: EventBus + Clone,
    B::EventType: From<D>,
    D: EventData + Send + Sync + TryFrom<B::EventType>,
{
    store: InMemoryEventStore<B>,
    _phantom: PhantomData<D>,
    id: Uuid,
    current_index: usize,
}

impl<B, D> InMemoryEventStoreStream<B, D>
where
    B: EventBus + Clone,
    B::EventType: From<D>,
    D: EventData + Send + Sync + TryFrom<B::EventType>,
{
    fn new(store: InMemoryEventStore<B>, id: Uuid) -> Self {
        Self {
            store,
            id,
            _phantom: PhantomData,
            current_index: 0,
        }
    }
}

impl<'a, B, D> EventStream<D, B::EventType> for InMemoryEventStoreStream<B, D>
where
    B: EventBus + Clone,
    B::EventType: From<D>,
    D: EventData + Send + Sync + TryFrom<B::EventType>,
{
}

impl<'a, B, D> Stream for InMemoryEventStoreStream<B, D>
where
    B: EventBus + Clone,
    B::EventType: From<D>,
    D: EventData + Send + Sync + TryFrom<B::EventType>,
{
    type Item = Result<Event<D>, D::Error>;

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
                        let res = D::try_from(data_p.clone());
                        return Poll::Ready(Some(res.map(|event| {
                            let data_d = event.clone();
                            event
                                .into_builder()
                                .data(Some(data_d))
                                .build()
                                .expect("Event to be buildable")
                        })));
                    }
                }
            }
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
#[derive(Debug, Clone)]
pub struct InMemoryEventBus<D, P>
where
    D: EventData + Send + Sync,
    P: Projector + Send + Sync,
    <P::Projection as Projection>::EventType: TryFrom<D> + EventData + Send + Sync,
{
    _phantom: PhantomData<D>,
    projectors: Arc<Mutex<Vec<P>>>,
}

impl<D, P> InMemoryEventBus<D, P>
where
    D: EventData + Send + Sync,
    P: Projector + Send + Sync,
    <P::Projection as Projection>::EventType: TryFrom<D> + EventData + Send + Sync,
{
    /// Creates a new in-memory bus
    pub fn new() -> Self {
        InMemoryEventBus {
            _phantom: PhantomData,
            projectors: Arc::new(Mutex::new(vec![])),
        }
    }
}

/// Errors for the InMemoryEventBus
#[derive(Debug, thiserror::Error)]
pub enum InMemoryEventBusError {}

impl<D, P> EventBus for InMemoryEventBus<D, P>
where
    D: EventData + Send + Sync,
    P: Projector + Send + Sync,
    <P::Projection as Projection>::EventType: TryFrom<D> + EventData + Send + Sync,
    <<P::Projection as Projection>::EventType as TryFrom<D>>::Error: std::error::Error,
{
    type Error = InMemoryEventBusError;
    type EventType = D;
    type ProjectorType = P;
    fn publish(
        &self,
        event: Event<Self::EventType>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send {
        async move {
            let projectors = self.projectors.lock().await;
            for projector in projectors.iter() {
                let data = event.data.clone().map(|d| d.try_into().unwrap());
                let event_for_projection = event.clone().into_builder().data(data).build().unwrap();
                projector.project(event_for_projection).await.unwrap();
            }
            Ok(())
        }
    }

    fn subscribe(
        &self,
        projector: Self::ProjectorType,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send
    where
        <<Self::ProjectorType as Projector>::Projection as Projection>::EventType:
            TryFrom<Self::EventType>,
    {
        async {
            let mut projectors = self.projectors.lock().await;
            projectors.push(projector);
            Ok(())
        }
    }
}
