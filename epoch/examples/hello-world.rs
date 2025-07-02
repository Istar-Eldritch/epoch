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

#[subset_enum(UserEvent, UserCreated, UserNameUpdated)]
#[subset_enum(UserEvento, UserCreated, UserNameUpdated)]
#[derive(Debug, Clone, serde::Serialize, EventData)]
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

    fn append_events(&mut self, stream_id: Uuid, events: Vec<Event<D>>) {
        self.events.extend_from_slice(&events);
        self.stream_events
            .entry(stream_id)
            .or_insert_with(Vec::new)
            .extend(events.iter().map(|e| e.id));
    }
}

#[async_trait::async_trait]
impl<P: EventData + Send + Sync> EventStoreBackend for VirtualEventStore<P> {
    type EventType = P;
    async fn fetch_stream<D: EventData + Send + Sync + TryFrom<Self::EventType>>(
        &mut self,
        stream_id: Uuid,
    ) -> Result<impl EventStream<D>, EventStreamFetchError>
    where
        P: From<D>,
    {
        Ok(VirtualEventStoreStream::<'_, Self::EventType, D>::new(
            self, stream_id,
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
        self.store.append_events(self.id, events);
        Ok(())
    }
}

struct VirtualEventStoreStream<'a, P: EventData + From<D>, D: EventData + Send + Sync + TryFrom<P>>
{
    store: &'a mut VirtualEventStore<P>,
    _phantom: PhantomData<D>,
    id: Uuid,
    current_index: usize,
}

impl<'a, P: EventData + From<D>, D: EventData + Send + Sync + TryFrom<P>>
    VirtualEventStoreStream<'a, P, D>
{
    fn new(store: &'a mut VirtualEventStore<P>, id: Uuid) -> Self {
        Self {
            store,
            id,
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
        // We need to use unsafe to get a mutable reference to the fields of the `!Unpin` struct.
        // This is safe because we are not moving the `VirtualEventStoreStream` itself.
        let this = unsafe { self.get_unchecked_mut() };
        let stream_events = this.store.stream_events.get(&this.id);

        if let Some(event_ids) = stream_events {
            if this.current_index < event_ids.len() {
                let event_id = event_ids[this.current_index];
                this.current_index += 1;

                // Find the actual event in the store's main events vector
                let event = this.store.events.iter().find(|e| e.id == event_id);

                if let Some(event) = event {
                    // Convert event P to D if possible
                    if let Some(data_p) = &event.data {
                        if let Ok(data_d) = D::try_from(data_p.clone()) {
                            let converted_event = data_d.into_event(event.clone());

                            return Poll::Ready(Some(converted_event));
                        }
                    }
                }
            }
        }
        Poll::Ready(None)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut store = VirtualEventStore::<ApplicationEvent>::new();
    let mut stream = store.fetch_stream::<UserEvent>(Uuid::new_v4()).await?;

    stream.append_to_stream(&vec![]).await?;

    // Explicitly call event_type to ensure EventData is implemented
    // let _event_type = UserEvent::UserCreated { id: Uuid::new_v4(), name: "Test".to_string() }.event_type();

    let user_event_instance = UserEvent::UserCreated {
        id: Uuid::new_v4(),
        name: "Debug Test".to_string(),
    };
    println!("Debug output of UserEvent: {:?}", user_event_instance);

    println!("Hello, from examples!");

    Ok(())
}
