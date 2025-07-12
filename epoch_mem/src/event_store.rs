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
    #[error("Event version ({0}) doesn't match stream version ({0})")]
    VersionMismatch(u64, u64),
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
    fn read_events(
        &self,
        stream_id: Uuid,
    ) -> impl Future<Output = Result<InMemoryEventStoreStream<B>, Self::Error>> + Send {
        log::debug!("Reading events for stream_id: {}", stream_id);
        let store = self.clone();
        async move { Ok(InMemoryEventStoreStream::<B>::new(store, stream_id)) }
    }

    fn store_event(
        &self,
        event: Event<Self::EventType>,
    ) -> impl Future<Output = Result<Event<Self::EventType>, Self::Error>> + Send {
        async move {
            let mut data = self.data.lock().await;
            let stream_id = event.stream_id;

            let mut version: u64 = *data.stream_version.get(&stream_id).unwrap_or(&0);

            if event.stream_version != version {
                log::debug!(
                    "Event version mismatch for stream_id: {}. Expected: {}, Got: {}",
                    stream_id,
                    version,
                    event.stream_version
                );
                Err(InMemoryEventStoreBackendError::VersionMismatch(
                    event.stream_version,
                    version,
                ))?;
            }
            log::debug!(
                "Event version check passed for stream_id: {}. Version: {}",
                stream_id,
                version
            );

            version += 1;

            data.stream_version.insert(stream_id, version);

            let event_id = event.id;
            data.events.insert(event_id, event.clone());
            data.stream_events
                .entry(stream_id)
                .or_insert_with(Vec::new)
                .extend(&[event_id]);
            log::debug!(
                "Event stored successfully for stream_id: {}, event_id: {}",
                stream_id,
                event_id
            );
            self.bus()
                .publish(event.clone())
                .await
                .map_err(|_e| InMemoryEventStoreBackendError::PublishEvent)?;
            Ok(event)
        }
    }
}

/// An in-memory event store stream.
pub struct InMemoryEventStoreStream<B>
where
    B: EventBus + Clone,
{
    id: Uuid,
    store: InMemoryEventStore<B>,
    current_index: usize,
}

impl<B> InMemoryEventStoreStream<B>
where
    B: EventBus + Clone,
{
    fn new(store: InMemoryEventStore<B>, id: Uuid) -> Self {
        Self {
            store,
            id,
            current_index: 0,
        }
    }
}

impl<'a, B> EventStream<B::EventType> for InMemoryEventStoreStream<B> where B: EventBus + Clone {}

impl<'a, B> Stream for InMemoryEventStoreStream<B>
where
    B: EventBus + Clone,
{
    type Item = Result<Event<B::EventType>, Box<dyn std::error::Error + Send>>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // We need to use unsafe to get a mutable reference to the fields of the `!Unpin` struct.
        // This is safe because we are not moving the `VirtualEventStoreStream` itself.
        let this = unsafe { self.get_unchecked_mut() };

        let data = match this.store.data.try_lock() {
            Ok(guard) => {
                log::debug!("InMemoryEventStoreStream: poll_next - Acquired lock");
                guard
            },
            Err(_) => {
                log::debug!("InMemoryEventStoreStream: poll_next - Lock contention, returning Poll::Pending");
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
            log::debug!("InMemoryEventStoreStream: poll_next - No more events in stream, returning Poll::Ready(None)");
        } else {
            log::debug!("InMemoryEventStoreStream: poll_next - No events found for stream_id: {}", this.id);
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
    D: EventData + Send + Sync,
{
    _phantom: PhantomData<D>,
    projections: Arc<Mutex<Vec<Arc<Mutex<dyn Projection<D>>>>>>,
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
                let mut projection = projection.lock().await;
                projection.apply(&event).await.unwrap_or_else(|e| {
                    log::error!("Error applying event: {:?}", e);
                    //TODO: Retry mechanism and dead letter queue
                });
            }
            Ok(())
        })
    }

    fn subscribe(
        &self,
        projector: Arc<Mutex<dyn Projection<Self::EventType>>>,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send>> {
        log::debug!("Subscribing projector to InMemoryEventBus");
        let projectors = self.projections.clone();
        Box::pin(async move {
            let mut projectors = projectors.lock().await;
            projectors.push(projector);
            Ok(())
        })
    }
}
