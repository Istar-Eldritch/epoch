//! This module defines the `EventStoreBackend` trait, which provides an interface for interacting
//! with an event store. It includes methods for reading events from a stream and appending new
//! events to a stream. This module also defines the `EventBus` trait, which allows for publishing
//! and subscribing to events.

use crate::event::{Event, EventData};
use crate::prelude::Projection;
use crate::projection::Projector;
use futures_core::Stream;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use uuid::Uuid;

/// A trait that defines the behavior of an event stream.
pub trait EventStream<D, P>: Stream<Item = Result<Event<D>, D::Error>>
where
    D: EventData + Send + Sync + TryFrom<P>,
{
}

/// A trait that defines the behavior of a storage backend.
pub trait EventStoreBackend {
    /// The type of event stored in this store
    type EventType: EventData + Send + Sync;

    /// The error when an event store operation fails
    type Error: std::error::Error;

    /// Fetches a stream from the storage backend.
    fn read_events<D>(
        &self,
        stream_id: Uuid,
    ) -> impl Future<Output = Result<impl EventStream<D, Self::EventType>, Self::Error>> + Send
    where
        D: TryFrom<Self::EventType> + EventData + Send + Sync,
        Self::EventType: From<D>;

    /// Appends events to a stream.
    fn store_event<D>(
        &self,
        event: Event<D>,
    ) -> impl Future<Output = Result<Event<Self::EventType>, Self::Error>> + Send
    where
        D: TryFrom<Self::EventType> + EventData + Send + Sync,
        Self::EventType: From<D>;
}

/// A trait that can be used to project events from an event bus.
pub trait DynProjector<E: EventData>: Send + Sync {
    /// Projects an event.
    fn project<'a, 'b>(
        &'a self,
        event: &Event<E>,
    ) -> Pin<
        Box<dyn Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send + 'b>,
    >
    where
        'a: 'b,
        E: 'b;
}

impl<P, E> DynProjector<E> for P
where
    E: EventData + Send + Sync,
    P: Projector + Send + Sync,
    <P::Projection as Projection>::EventType: TryFrom<E> + EventData,
    <<P::Projection as Projection>::EventType as TryFrom<E>>::Error:
        std::error::Error + Send + Sync + 'static,
    <P as Projector>::Error: std::error::Error + Send + Sync + 'static,
{
    fn project<'a, 'b>(
        &'a self,
        event: &Event<E>,
    ) -> Pin<
        Box<dyn Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send + 'b>,
    >
    where
        'a: 'b,
        E: 'b,
    {
        let event = event.clone();
        Box::pin(async move {
            if let Some(data) = event.data.clone() {
                if let Ok(converted_data) = <P::Projection as Projection>::EventType::try_from(data)
                {
                    let event_for_projection = event
                        .clone()
                        .into_builder()
                        .data(Some(converted_data))
                        .build()
                        .expect("Event to be buildable");

                    return self
                        .project(event_for_projection)
                        .await
                        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>);
                }
            }
            Ok(())
        })
    }
}

/// A trait that defines the behavior of an event bus.
pub trait EventBus {
    /// The type of event that can be published to this event bus.
    type EventType: EventData + Send + Sync;
    /// The errors when an event bus operation fails
    type Error: std::error::Error;
    /// Publishes events to the event bus.
    fn publish<'a>(
        &'a self,
        event: Event<Self::EventType>,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>>;

    /// Allows to subscribe to events
    fn subscribe(
        &self,
        projector: Arc<dyn DynProjector<Self::EventType>>,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send>>;
}
