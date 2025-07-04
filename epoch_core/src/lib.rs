//! # Epoch

#![deny(missing_docs)]

mod event;

use std::collections::HashMap;

use async_trait::async_trait;
pub use event::{EnumConversionError, Event, EventBuilder, EventBuilderError, EventData};
use futures_core::Stream;
use tokio_stream::StreamExt;
use uuid::Uuid;

pub mod prelude {
    //! The prelude module for the `epoch` crate.
    pub use super::{
        EnumConversionError, Event, EventBuilder, EventBuilderError, EventBus, EventData,
        EventStoreBackend, EventStream, Projection,
    };
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

/// A trait that defines the behavior of a projection.
/// A projection is a read-model that is built from a stream of events.
pub trait Projection<E: EventData>: Sized {
    /// If the event can't be applied, this error is returned
    type ProjectionError: std::error::Error;
    /// Creates a new projection from an event
    fn new(event: Event<E>) -> Result<Self, impl std::error::Error>;
    /// Updates the projection with a single event.
    fn apply(self, event: Event<E>) -> Result<Self, impl std::error::Error>;
    /// Retrieves the projection id from the event.
    fn get_projection_id(event: &Event<E>) -> Option<Uuid>;
}

/// A trait that wraps a Projection and handles persistance, cache and other related logic
// #[async_trait::async_trait]
// pub trait Projector<D: EventData, P: Projection<D>> {
//     async fn get_snapshot(&self, event: &Event<D>) -> Option<P>;
//
//     /// Projects a stream of events onto a projection.
//     async fn project<'a>(&'a self, event: Event<D>) -> Result<P, P::ProjectionError>
//     where
//         D: EventData + Send + Sync + 'a,
//         P: Projection<D>,
//     {
//         if let Some(snapshot) = self.get_snapshot(&event).await {
//             snapshot.apply(&event)
//         } else {
//             P::new(&event)
//         }
//     }
// }

/// A trait that defines the behavior of a projector.
/// A projector is responsible for creating and updating projections from events.
#[async_trait]
pub trait Projector {
    /// The type of event that this projector can handle.
    type EventType: EventData + Send;
    /// The type of projection that this projector creates and updates.
    type ProjectionType: Projection<Self::EventType>;
    /// Retrieves a snapshot of the projection for a given event.
    /// This is used to optimize projection by starting from a known state.
    async fn get_snapshot(
        &self,
        id: Uuid,
    ) -> Result<Option<Self::ProjectionType>, impl std::error::Error>;
    /// Persist snapshot
    async fn persist_snapshot(
        &self,
        id: Uuid,
        snapshot: Self::ProjectionType,
    ) -> Result<(), impl std::error::Error>;
    /// Projects a single event onto a projection.
    /// If a snapshot is available, it will be used as the starting point.
    /// Otherwise, a new projection will be created.
    async fn project<'a>(
        &'a self,
        event: Event<Self::EventType>,
    ) -> Result<Self::ProjectionType, Box<dyn std::error::Error + 'a>> {
        if let Some(id) = Self::ProjectionType::get_projection_id(&event) {
            if let Some(snapshot) = self.get_snapshot(id).await? {
                Ok(snapshot.apply(event)?)
            } else {
                Ok(Self::ProjectionType::new(event)?)
            }
        } else {
            Ok(Self::ProjectionType::new(event)?)
        }
    }
}

struct MemProjector<P: Sized> {
    entities: HashMap<Uuid, P>,
}

// impl<D: EventData, P: Projection<D>> Projector<D, P> for MemProjector<P> {
//     fn get_snapshot(&self, event: &Event<D>) -> Option<P> {
//         todo!()
//     }
// }
