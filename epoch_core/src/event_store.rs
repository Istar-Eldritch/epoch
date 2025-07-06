//! This module defines the `EventStoreBackend` trait, which provides an interface for interacting
//! with an event store. It includes methods for reading events from a stream and appending new
//! events to a stream. This module also defines the `EventBus` trait, which allows for publishing
//! and subscribing to events.

use crate::event::{Event, EventData};
use crate::prelude::Projection;
use crate::projection::Projector;
use futures_core::Stream;
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

/// A trait that defines the behavior of an event bus.
pub trait EventBus {
    /// The type of event that can be published to this event bus.
    type EventType: EventData + Send + Sync;
    /// The errors when an event bus operation fails
    type Error: std::error::Error;
    /// The Type of projector that can subscribe
    type ProjectorType: Projector;
    //where
    //    <<Self::ProjectorType as Projector>::Projection as Projection>::EventType:
    //        TryFrom<Self::EventType>;
    /// Publishes events to the event bus.
    fn publish<D>(&self, event: Event<D>) -> impl Future<Output = Result<(), Self::Error>> + Send
    where
        D: EventData + Send + Sync,
        <<Self::ProjectorType as Projector>::Projection as Projection>::EventType: From<D>,
        Self::EventType: From<D>;

    /// Allows to subscribe to events
    fn subscribe(
        &self,
        projector: Self::ProjectorType,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send
    where
        <<Self::ProjectorType as Projector>::Projection as Projection>::EventType:
            TryFrom<Self::EventType>;
}
