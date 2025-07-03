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
pub trait Projection<E: EventData>: Sized {
    /// If the event can't be applied, this error is returned
    type ProjectionError;
    /// Creates a new projection from an event
    fn new(event: &E) -> Result<Self, Self::ProjectionError>;
    /// Updates the projection with a single event.
    fn apply(self, event: &E) -> Result<Self, Self::ProjectionError>;
}

/// A projector is responsible for applying events to a projection.
pub struct Projector;

impl Projector {
    /// Projects a stream of events onto a projection.
    async fn project_impl<D, P, S>(
        snapshot: Option<P>,
        mut stream: S,
    ) -> Result<Option<P>, P::ProjectionError>
    where
        D: EventData,
        P: Projection<D>,
        S: futures_core::stream::Stream<Item = Event<D>> + Unpin,
    {
        let mut projection: Option<P> = snapshot;
        if projection.is_none() {
            if let Some(event) = stream.next().await {
                if let Some(data) = event.data {
                    projection = Some(P::new(&data)?);
                }
            }
        }
        while let Some(event) = stream.next().await {
            if let Some(data) = &event.data {
                projection = projection.map(|p: P| p.apply(data)).transpose()?;
            }
        }
        Ok(projection)
    }
    /// Projects a stream of events onto a projection.
    pub async fn project<D, P, S>(stream: S) -> Result<Option<P>, P::ProjectionError>
    where
        D: EventData,
        P: Projection<D>,
        S: futures_core::stream::Stream<Item = Event<D>> + Unpin,
    {
        Projector::project_impl::<D, P, S>(None, stream).await
    }

    /// Projects a stream of events onto a projection.
    pub async fn project_on_snapshot<D, P, S>(
        snapshot: P,
        stream: S,
    ) -> Result<P, P::ProjectionError>
    where
        D: EventData,
        P: Projection<D>,
        S: futures_core::stream::Stream<Item = Event<D>> + Unpin,
    {
        Ok(Projector::project_impl::<D, P, S>(Some(snapshot), stream)
            .await?
            .unwrap())
    }
}
