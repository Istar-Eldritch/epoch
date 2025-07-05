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
pub struct MemEventStore<D: EventData> {
    data: Arc<Mutex<EventStoreData<D>>>,
}

impl<D: EventData> Default for MemEventStore<D> {
    fn default() -> Self {
        Self {
            data: Arc::new(Mutex::new(EventStoreData {
                events: HashMap::new(),
                stream_events: HashMap::new(),
                sequence_number: 0,
            })),
        }
    }
}

impl<D: EventData> MemEventStore<D> {
    /// Creates a new `MemEventStore`.
    pub fn new() -> Self {
        Self::default()
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
impl<P: EventData + Send + Sync> EventStoreBackend for MemEventStore<P> {
    type EventType = P;
    type AppendToStreamError = EventStreamAppendError;
    async fn read_events<'a, D: EventData + Send + Sync + TryFrom<Self::EventType>>(
        &'a self,
        stream_id: Uuid,
    ) -> Result<MemEventStoreStream<'a, P, D>, EventStreamFetchError>
    where
        P: From<D>,
    {
        Ok(MemEventStoreStream::<'_, Self::EventType, D>::new(
            self, stream_id,
        ))
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

#[async_trait::async_trait]
impl<'a, P: EventData + From<D> + Send + Sync, D: EventData + Send + Sync + TryFrom<P>>
    EventStream<D> for MemEventStoreStream<'a, P, D>
{
}

/// An in--memory event store stream.
pub struct MemEventStoreStream<'a, P: EventData + From<D>, D: EventData + Send + Sync + TryFrom<P>>
{
    store: &'a MemEventStore<P>,
    _phantom: PhantomData<D>,
    id: Uuid,
    current_index: usize,
}

impl<'a, P: EventData + From<D>, D: EventData + Send + Sync + TryFrom<P>>
    MemEventStoreStream<'a, P, D>
{
    fn new(store: &'a MemEventStore<P>, id: Uuid) -> Self {
        Self {
            store,
            id,
            _phantom: PhantomData,
            current_index: 0,
        }
    }
}

impl<'a, P: EventData + Send + Sync + From<D>, D: EventData + Send + Sync + TryFrom<P>> Stream
    for MemEventStoreStream<'a, P, D>
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
