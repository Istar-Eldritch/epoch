//! # Epoch

#![deny(missing_docs)]

mod event;

pub use event::{EnumConversionError, Event, EventBuilder, EventBuilderError, EventData};
use futures_core::Stream;
use tokio_stream::StreamExt;
use uuid::Uuid;

pub mod prelude {
    //! The prelude module for the `epoch` crate.
    pub use super::{
        EnumConversionError, Event, EventBuilder, EventBuilderError, EventData, EventStoreBackend,
        EventStream, Projection, Projector,
    };
}

/// A trait that defines the behavior of an event stream.
#[async_trait::async_trait]
pub trait EventStream<D: EventData>: Stream<Item = Event<D>> {
    /// The error returned by append to stream
    type AppendToStreamError: std::error::Error;
    /// Appends events to a stream.
    async fn append_to_stream(&self, events: &[Event<D>]) -> Result<(), Self::AppendToStreamError>;
}

/// A trait that defines the behavior of a storage backend.
#[async_trait::async_trait]
pub trait EventStoreBackend {
    /// The type of event stored in this store
    type EventType: EventData;
    /// Fetches a stream from the storage backend.
    async fn fetch_stream<D: TryFrom<Self::EventType> + EventData + Send + Sync>(
        &self,
        stream_id: Uuid,
    ) -> Result<impl EventStream<D>, impl std::error::Error>
    where
        Self::EventType: From<D>;
}

/// A trait that defines the behavior of a projection.
/// A projection is a read-model that is built from a stream of events.
pub trait Projection<E: EventData>: Default {
    /// Updates the projection with a single event.
    fn apply(self, event: &E) -> Self;
}

/// A projector is responsible for applying events to a projection.
pub struct Projector;

impl Projector {
    /// Projects a stream of events onto a projection.
    pub async fn project<E, P, S>(stream: &mut S) -> P
    where
        E: EventData,
        P: Projection<E>,
        S: futures_core::stream::Stream<Item = Event<E>> + Unpin,
    {
        let mut projection = P::default();
        while let Some(event) = stream.next().await {
            if let Some(data) = &event.data {
                projection = projection.apply(data);
            }
        }
        projection
    }
}
