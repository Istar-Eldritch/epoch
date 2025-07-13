//! This module defines the `EventStoreBackend` trait, which provides an interface for interacting
//! with an event store. It includes methods for reading events from a stream and appending new
//! events to a stream. This module also defines the `EventBus` trait, which allows for publishing
//! and subscribing to events.

use crate::event::{Event, EventData};
use crate::prelude::Projection;
use futures_core::Stream;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

/// A trait that defines the behavior of an event stream.
pub trait EventStream<D>:
    Stream<Item = Result<Event<D>, Box<dyn std::error::Error + Send + Sync>>> + Send
where
    D: EventData + Send + Sync,
{
}

/// A trait that defines the behavior of a storage backend.
pub trait EventStoreBackend<'a>: Send + Sync {
    /// The type of event stored in this store
    type EventType: EventData + Send + Sync;

    /// The error when an event store operation fails
    type Error: std::error::Error + Send + Sync + 'a;

    /// Fetches a stream from the storage backend.
    fn read_events(
        &'a self,
        stream_id: Uuid,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Pin<Box<dyn EventStream<Self::EventType> + Send + 'a>>,
                        Self::Error,
                    >,
                > + Send
                + 'a,
        >,
    >;

    /// Appends events to a stream.
    fn store_event(
        &'a self,
        event: Event<Self::EventType>,
    ) -> Pin<Box<dyn Future<Output = Result<Event<Self::EventType>, Self::Error>> + Send + 'a>>;
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
        projector: Arc<Mutex<dyn Projection<Self::EventType>>>,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send>>;
}
