//! # Epoch

#![deny(missing_docs)]

mod event;

use std::{collections::HashMap, sync::Arc};

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

/// A trait that defines an error from a projector.
pub trait ProjectorError: std::error::Error + Send + Sync {
    /// The projection error type.
    type ProjectionError: ProjectionError;
    /// The store error type.
    type StoreError: ProjectionStoreError;
    /// Creates an error from a projection error.
    fn from_projection_error(err: Self::ProjectionError) -> Self;
    /// Creates an error from a store error.
    fn from_store_error(err: Self::StoreError) -> Self;
}

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
    type Error: ProjectorError<
            ProjectionError = <Self::Projection as Projection>::Error,
            StoreError = <Self::Store as ProjectionStore>::Error,
        >;
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
        Self: Send + Sync,
    {
        async {
            let id = Self::Projection::get_id_from_event(&event);
            let projection = match id {
                Some(id) => {
                    let snapshot = self
                        .get_store()
                        .fetch_by_id(&id)
                        .await
                        .map_err(Self::Error::from_store_error)?;
                    match snapshot {
                        Some(snapshot) => snapshot
                            .apply(event)
                            .map_err(Self::Error::from_projection_error)?,
                        None => Self::Projection::new(event)
                            .map_err(Self::Error::from_projection_error)?,
                    }
                }
                None => Self::Projection::new(event).map_err(Self::Error::from_projection_error)?,
            };

            self.get_store()
                .store(projection)
                .await
                .map_err(Self::Error::from_store_error)?;
            Ok(())
        }
    }
}

/// Error thrown by a projection store
pub trait ProjectionStoreError: std::error::Error + Send + Sync {}

/// A trait defining the behavior of a projection store
pub trait ProjectionStore: Send {
    /// The errors returned by this store
    type Error: ProjectionStoreError;
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

/// The error of the in-memory store
#[derive(Debug, thiserror::Error)]
#[error("MemProjectionStoreInfallibleError")]
pub struct MemProjectionStoreInfallibleError;

impl ProjectionStoreError for MemProjectionStoreInfallibleError {}

impl<P: Sized + Projection + Clone> ProjectionStore for MemProjectionStore<P> {
    type Error = MemProjectionStoreInfallibleError;
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

/// A projector that uses a `ProjectionStore` to store projections.
pub struct StoreProjector<S: ProjectionStore>(S);

impl<S> StoreProjector<S>
where
    S: ProjectionStore,
{
    /// Creates a new instance of the projector
    pub fn new(store: S) -> Self {
        StoreProjector(store)
    }
}

/// Errors that may happen when working with the in-memory projector
#[derive(Debug, thiserror::Error)]
pub enum MemoryProjectorError<P, S>
where
    P: ProjectionError,
    S: ProjectionStoreError,
{
    /// An error originating from the Projection
    #[error("Projection error: {0}")]
    Projection(P),
    /// An error originated from the projeciton store
    #[error("Projection store error: {0}")]
    Store(S),
}

impl<P, S> ProjectorError for MemoryProjectorError<P, S>
where
    P: ProjectionError,
    S: ProjectionStoreError,
{
    type ProjectionError = P;
    type StoreError = S;

    fn from_projection_error(err: Self::ProjectionError) -> Self {
        Self::Projection(err)
    }

    fn from_store_error(err: Self::StoreError) -> Self {
        Self::Store(err)
    }
}

impl<S> Projector for StoreProjector<S>
where
    S: ProjectionStore,
    <S as ProjectionStore>::Entity: Projection,
{
    type Projection = S::Entity;
    type Error = MemoryProjectorError<
        <<Self as Projector>::Projection as Projection>::Error,
        <<Self as Projector>::Store as ProjectionStore>::Error,
    >;
    type Store = S;
    fn get_store(&self) -> &Self::Store {
        &self.0
    }
}
