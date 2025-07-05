use crate::event::{Event, EventData};
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
