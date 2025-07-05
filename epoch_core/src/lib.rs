//! # Epoch

#![deny(missing_docs)]

mod event;

use std::{collections::HashMap, convert::Infallible, sync::Arc};

pub use event::{EnumConversionError, Event, EventBuilder, EventBuilderError, EventData};
use futures_core::Stream;
use tokio::sync::Mutex;
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

/// DOCS
pub trait ProjectionError: std::error::Error + Send + Sync {}

/// A trait that defines the behavior of a projection.
/// A projection is a read-model that is built from a stream of events.
pub trait Projection: Sized + Send + Sync {
    /// DOCS
    type EventType: EventData + Send;
    /// If the event can't be applied, this error is returned
    type Error: ProjectionError;
    /// Creates a new projection from an event
    fn new(event: Event<Self::EventType>) -> Result<Self, Self::Error>;
    /// Updates the projection with a single event.
    fn apply(self, event: Event<Self::EventType>) -> Result<Self, Self::Error>;
    /// Retrieves the projection id from the event.
    fn get_id_from_event(event: &Event<Self::EventType>) -> Option<Uuid>;
    /// Get the id of the projection
    fn get_id(&self) -> &Uuid;
}

/// A trait that defines the behavior of a projector.
/// A projector is responsible for creating and updating projections from events.
pub trait Projector {
    /// DOCS
    type Error;
    /// DOCS
    type Projection: Projection + Send;
    /// DOCS
    type Store: ProjectionStore<Entity = Self::Projection> + Send;
    /// Store
    fn get_store(&self) -> &Self::Store;
    /// Projects a single event onto a projection.
    /// If a snapshot is available, it will be used as the starting point.
    /// Otherwise, a new projection will be created.
    fn project(
        &self,
        event: Event<<<Self as Projector>::Projection as Projection>::EventType>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send
    where
        <Self as Projector>::Error: From<<<Self as Projector>::Projection as Projection>::Error>,
        <Self as Projector>::Error: From<<<Self as Projector>::Store as ProjectionStore>::Error>,
        Self: Send + Sync,
    {
        async {
            let id = Self::Projection::get_id_from_event(&event);
            let projection = match id {
                Some(id) => {
                    let snapshot = self.get_store().fetch_by_id(&id).await?;
                    match snapshot {
                        Some(snapshot) => snapshot.apply(event)?,
                        None => Self::Projection::new(event)?,
                    }
                }
                None => Self::Projection::new(event)?,
            };

            self.get_store().store(projection).await?;
            Ok(())
        }
    }
}

/// A trait defining the behavior of a projection store
pub trait ProjectionStore: Send {
    /// The errors returned by this store
    type Error;
    /// The entity type to store on the store
    type Entity: Projection;
    /// Finds a projection in the store by its id
    fn fetch_by_id(
        &self,
        id: &Uuid,
    ) -> impl Future<Output = Result<Option<Self::Entity>, Self::Error>> + Send;
    /// Stores a projection in the store
    fn store(
        &self,
        projection: Self::Entity,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// An in-memory store for Projections
#[derive(Clone, Debug)]
pub struct MemProjectionStore<P: Sized + Send> {
    entities: Arc<Mutex<HashMap<Uuid, P>>>,
}

impl<P> MemProjectionStore<P>
where
    P: Sized + Send,
{
    /// Create a new instance of a MemProjectionStore
    pub fn new() -> Self {
        MemProjectionStore {
            entities: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl<P: Sized + Projection + Clone> ProjectionStore for MemProjectionStore<P> {
    type Error = Infallible;
    type Entity = P;
    fn store(&self, projection: P) -> impl Future<Output = Result<(), Self::Error>> + Send {
        async {
            let mut entities = self.entities.lock().await;
            entities.insert(projection.get_id().clone(), projection);
            Ok(())
        }
    }
    fn fetch_by_id(
        &self,
        id: &Uuid,
    ) -> impl Future<Output = Result<Option<P>, Self::Error>> + Send {
        async {
            let entities = self.entities.lock().await;
            let entity: Option<P> = entities.get(id).map(|d| d.clone());
            Ok(entity)
        }
    }
}

/// An in-memory projector
pub struct MemProjector<S: ProjectionStore>(S);

impl<S> MemProjector<S>
where
    S: ProjectionStore,
{
    /// Creates a new instance of the projector
    pub fn new(store: S) -> Self {
        MemProjector(store)
    }
}

/// Errors that may happen when working with the in-memory projector
#[derive(Debug, thiserror::Error)]
pub enum MemoryProjectorError<P>
where
    P: ProjectionError,
{
    /// An error originating from the Projection
    #[error("Projection error: {0}")]
    Projection(#[from] P),
    /// An error that never happends
    #[error("This error should never happen: {0}")]
    Infallible(#[from] Infallible),
    // /// An error of unknown origin that was not handled specifically
    // #[error("Unexpected memory projector error: {0}")]
    // Unexpected(#[from] Box<dyn std::error::Error + Send>),
}

impl<S> Projector for MemProjector<S>
where
    S: ProjectionStore,
{
    type Projection = <<Self as Projector>::Store as ProjectionStore>::Entity;
    type Error = MemoryProjectorError<<<Self as Projector>::Projection as Projection>::Error>;
    type Store = S;
    fn get_store(&self) -> &Self::Store {
        &self.0
    }
}
