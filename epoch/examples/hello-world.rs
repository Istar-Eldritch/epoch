use std::collections::HashMap;

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

struct VirtualEventStore {
    events: Vec<Event<ApplicationEvent>>,
    stream_events: HashMap<Uuid, Vec<Uuid>>,
}

impl VirtualEventStore {
    fn new() -> Self {
        Self {
            events: Vec::new(),
            stream_events: HashMap::new(),
        }
    }
}

#[async_trait::async_trait]
impl EventStoreBackend for VirtualEventStore {
    type EventType = ApplicationEvent;
    async fn fetch_stream<E: TryFrom<Self::EventType> + EventData + Send + Sync>(
        &self,
        stream_id: Uuid,
    ) -> Result<impl EventStream<E, E::Error>, EventStreamFetchError> {
        let events: Vec<Result<Event<E>, E::Error>> = self
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
                                let data = event.data.map(|d| d.try_into()).transpose()?;
                                Ok(Event {
                                    id: event.id,
                                    created_at: event.created_at,
                                    purged_at: event.purged_at,
                                    sequence_number: event.sequence_number,
                                    event_type: event.event_type,
                                    actor_id: event.actor_id,
                                    purger_id: event.purger_id,
                                    data,
                                })
                            })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        Ok(VirtualEventStoreStream {
            events,
            current_index: 0,
        })
    }
}

#[async_trait::async_trait]
impl<D: EventData + Sync + Send, E> EventStream<D, E> for VirtualEventStoreStream<D, E> {
    async fn append_to_stream(
        &mut self,
        events: &[Event<D>],
    ) -> Result<(), EventStreamAppendError> {
        self.events
            .extend_from_slice(events.into_iter().map(|e| Ok(e)).collect());
        Ok(())
    }
}

struct VirtualEventStoreStream<D: EventData + Send + Sync, E> {
    events: Vec<Result<Event<D>, E>>,
    current_index: usize,
}

impl<E: EventData + Send + Sync, Error> VirtualEventStoreStream<E, Error> {
    fn new(events: Vec<Result<Event<E>, Error>>) -> Self {
        Self {
            events,
            current_index: 0,
        }
    }
}

impl<D: EventData + Send + Sync, E> Stream for VirtualEventStoreStream<D, E> {
    type Item = Result<Event<D>, E>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.current_index < self.events.len() {
            let event = self.events.get(self.current_index).map(|o| o.clone());
            unsafe {
                self.get_unchecked_mut().current_index += 1;
            }
            Poll::Ready(event)
        } else {
            Poll::Ready(None)
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let store = VirtualEventStore::new();
    let mut stream = store
        .fetch_stream::<ApplicationEvent>(Uuid::new_v4())
        .await?;

    println!("Hello, from examples!");

    Ok(())
}
