//! This module defines the `EventStoreBackend` trait, which provides an interface for interacting
//! with an event store. It includes methods for reading events from a stream and appending new
//! events to a stream. This module also defines the `EventBus` trait, which allows for publishing
//! and subscribing to events.

use crate::event::{Event, EventData};
use crate::projection::Projector;
use futures_core::Stream;
use uuid::Uuid;

/// A trait that defines the behavior of an event stream.
#[async_trait::async_trait]
pub trait EventStream<D: EventData>: Stream<Item = Event<D>> {}

/// A trait that defines the behavior of a storage backend.
#[async_trait::async_trait]
pub trait EventStoreBackend {
    /// The type of event stored in this store
    type EventType: EventData;
    /// Fetches a stream from the storage backend.
    async fn read_events<D: TryFrom<Self::EventType> + EventData + Send + Sync>(
        &self,
        stream_id: Uuid,
    ) -> Result<impl EventStream<D>, impl std::error::Error>
    where
        Self::EventType: From<D>;

    /// The error returned by append to stream
    type AppendToStreamError: std::error::Error;

    /// Appends events to a stream.
    async fn store_event<D>(
        &self,
        event: Event<D>,
    ) -> Result<Event<Self::EventType>, Self::AppendToStreamError>
    where
        D: EventData + Send + Sync,
        Self::EventType: From<D>;
}

/// A trait that defines the behavior of an event bus.
#[async_trait::async_trait]
pub trait EventBus {
    /// The type of event that can be published to this event bus.
    type EventType: EventData;
    /// The error returned by publish
    type PublishError: std::error::Error;
    /// Publishes events to the event bus.
    async fn publish(&self, event: Event<Self::EventType>) -> Result<(), Self::PublishError>;

    /// Allows to subscribe to events
    fn subscribe(&self, projector: impl Projector) -> ();
}
