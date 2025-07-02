use std::collections::HashMap;
use std::marker::PhantomData;

use epoch::prelude::*;
use epoch_derive::EventData;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::Stream;
use uuid::Uuid;

struct User {
    id: Uuid,
    name: String,
}

#[derive(Debug, Clone, serde::Serialize, EventData)]
#[subset_enum(UserEvent, UserCreated, UserNameUpdated)]
enum ApplicationEvent {
    UserCreated { id: Uuid, name: String },
    UserNameUpdated { id: Uuid, name: String },
}

struct VirtualEventStore<D: EventData> {
    events: Vec<Event<D>>,
    stream_events: HashMap<Uuid, Vec<Uuid>>,
}

impl<D: EventData> VirtualEventStore<D> {
    fn new() -> Self {
        Self {
            events: Vec::new(),
            stream_events: HashMap::new(),
        }
    }
}

#[async_trait::async_trait]
impl<P: EventData + Send + Sync> EventStoreBackend for VirtualEventStore<P> {
    type EventType = P;
    async fn fetch_stream<D: EventData + Send + Sync + TryFrom<Self::EventType>>(
        &self,
        stream_id: Uuid,
    ) -> Result<impl EventStream<D>, EventStreamFetchError>
    where
        P: From<D>,
    {
        let events: Vec<Event<D>> = self
            .stream_events
            .get(&stream_id)
            .map(|event_ids| {
                event_ids
                    .iter()
                    .filter_map(|event_id| {
                        self.events
                            .iter()
                            .find(|event| event.id == *event_id)
                            .cloned()
                            .map(|event| {
                                let data = todo!();
                                //event.data.map(|d| d.try_into()).transpose().expect("boom");
                                Event {
                                    id: event.id,
                                    created_at: event.created_at,
                                    purged_at: event.purged_at,
                                    sequence_number: event.sequence_number,
                                    event_type: event.event_type,
                                    actor_id: event.actor_id,
                                    purger_id: event.purger_id,
                                    data,
                                }
                            })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        Ok(VirtualEventStoreStream::<'_, Self::EventType, D>::new(
            &self,
        ))
    }
}

#[async_trait::async_trait]
impl<'a, P: EventData + From<D> + Send + Sync, D: EventData + Send + Sync + TryFrom<P>>
    EventStream<D> for VirtualEventStoreStream<'a, P, D>
{
    async fn append_to_stream(
        &mut self,
        events: &[Event<D>],
    ) -> Result<(), EventStreamAppendError> {
        let events: Vec<Event<P>> = events
            .into_iter()
            .map(|e| {
                let data: D = e.data.clone().unwrap();
                let data: P = data.into();
                let e: Event<P> = data.into_event(e.clone());
                e
            })
            .collect();
        // TODO: Use interior mutability
        // self.store.events.extend_from_slice(&events);
        Ok(())
    }
}

struct VirtualEventStoreStream<'a, P: EventData + From<D>, D: EventData + Send + Sync + TryFrom<P>>
{
    store: &'a VirtualEventStore<P>,
    _phantom: PhantomData<D>,
    current_index: usize,
}

impl<'a, P: EventData + From<D>, D: EventData + Send + Sync + TryFrom<P>>
    VirtualEventStoreStream<'a, P, D>
{
    fn new(store: &'a VirtualEventStore<P>) -> Self {
        Self {
            store,
            _phantom: PhantomData,
            current_index: 0,
        }
    }
}

impl<'a, P: EventData + Send + Sync + From<D>, D: EventData + Send + Sync + TryFrom<P>> Stream
    for VirtualEventStoreStream<'a, P, D>
{
    type Item = Event<D>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(None)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let store = VirtualEventStore::<ApplicationEvent>::new();
    let mut stream = store
        .fetch_stream::<ApplicationEvent>(Uuid::new_v4())
        .await?;

    println!("Hello, from examples!");

    Ok(())
}
