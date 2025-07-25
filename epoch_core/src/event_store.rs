//! This module defines the `EventStoreBackend` trait, which provides an interface for interacting
//! with an event store. It includes methods for reading events from a stream and appending new
//! events to a stream. This module also defines the `EventBus` trait, which allows for publishing
//! and subscribing to events.

use crate::event::{Event, EventData};
use async_trait::async_trait;
use futures_core::Stream;
use std::future::Future;
use std::pin::Pin;
use uuid::Uuid;

/// A trait that defines the behavior of an event stream.
pub trait EventStream<D>:
    Stream<Item = Result<Event<D>, Box<dyn std::error::Error + Send + Sync>>> + Send
where
    D: EventData + Send + Sync,
{
}

/// A trait that defines the behavior of a storage backend.
#[async_trait]
pub trait EventStoreBackend: Send + Sync {
    /// The type of event stored in this store
    type EventType: EventData + Send + Sync;

    /// The error when an event store operation fails
    type Error: std::error::Error + Send + Sync;

    /// Fetches a stream from the storage backend.
    async fn read_events(
        &self,
        stream_id: Uuid,
    ) -> Result<Pin<Box<dyn EventStream<Self::EventType> + Send + 'life0>>, Self::Error>;

    /// Appends events to a stream.
    async fn store_event(&self, event: Event<Self::EventType>) -> Result<(), Self::Error>;
}

/// A trait that defines the behavior of an event bus.
pub trait EventBus {
    /// The type of event that can be published to this event bus.
    type EventType: EventData + Send + Sync + 'static;
    /// The errors when an event bus operation fails
    type Error: std::error::Error;
    /// Publishes events to the event bus.
    fn publish<'a>(
        &'a self,
        event: Event<Self::EventType>,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>>;

    /// Allows to subscribe to events
    fn subscribe<T>(
        &self,
        projector: T,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send>>
    where
        T: EventObserver<Self::EventType> + Send + Sync + 'static;
}

///  Traits to defined observers to the event bus
#[async_trait]
pub trait EventObserver<ED>: Send + Sync
where
    ED: EventData + Send + Sync,
{
    /// Reacts to events published in an event bus
    async fn on_event(
        &self,
        event: Event<ED>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}
