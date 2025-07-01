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

#[derive(serde::Serialize, Clone, EventData)]
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
    async fn fetch_stream<E: From<Self::EventType> + EventData + Clone + Send + Sync>(
        &self,
        stream_id: Uuid,
    ) -> Result<impl EventStream<E>, EventStreamFetchError> {
        let events: Vec<Event<E>> = self
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
                            .map(|event| Event {
                                id: event.id,
                                created_at: event.created_at,
                                purged_at: event.purged_at,
                                sequence_number: event.sequence_number,
                                event_type: event.event_type,
                                actor_id: event.actor_id,
                                purger_id: event.purger_id,
                                data: event.data.map(|d| d.into()),
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
impl<E: EventData + Clone + Sync + Send> EventStream<E> for VirtualEventStoreStream<E> {
    async fn append_to_stream(
        &mut self,
        events: &[Event<E>],
    ) -> Result<(), EventStreamAppendError> {
        self.events.extend_from_slice(events);
        Ok(())
    }
}

struct VirtualEventStoreStream<E: EventData + Clone + Send + Sync> {
    events: Vec<Event<E>>,
    current_index: usize,
}

impl<E: EventData + Clone + Send + Sync> VirtualEventStoreStream<E> {
    fn new(events: Vec<Event<E>>) -> Self {
        Self {
            events,
            current_index: 0,
        }
    }
}

impl<E: EventData + Clone + Send + Sync> Stream for VirtualEventStoreStream<E> {
    type Item = Event<E>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.current_index < self.events.len() {
            let event = self.events[self.current_index].clone();
            unsafe {
                self.get_unchecked_mut().current_index += 1;
            }
            Poll::Ready(Some(event))
        } else {
            Poll::Ready(None)
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let store = VirtualEventStore::new();
    let mut stream = store.fetch_stream::<UserEvent>(Uuid::new_v4()).await?;
    println!("Hello, from examples!");

    Ok(())
}
