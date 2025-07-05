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

/// DOS
#[derive(Debug, thiserror::Error)]
pub enum ProjectorError {
    /// DOCS
    #[error("An unexpected eror happened: {0}")]
    Unexpected(#[from] Box<dyn std::error::Error + Send + Sync>),
    /// DOCS
    #[error("A projection error happened: {0}")]
    Projection(Box<dyn ProjectionError>),
    /// DOCS
    #[error("A storage error happened: {0}")]
    Storage(Box<dyn ProjectionStorageError>),
}

impl<PE: ProjectionError + 'static> From<PE> for ProjectorError {
    fn from(error: PE) -> Self {
        ProjectorError::Projection(Box::new(error))
    }
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
        <<Self as Projector>::Store as ProjectionStore>::Error: ProjectionError + 'static,
        <<Self as Projector>::Projection as Projection>::Error: 'static,
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

/// Errors from the projection store
pub trait ProjectionStorageError: std::error::Error + Send + Sync {}

/// A trait defining the behavior of a projection store
pub trait ProjectionStore: Send {
    /// The errors returned by this store
    type Error: ProjectionStorageError;
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

struct MemProjectionStore<P: Sized + Send> {
    entities: Arc<Mutex<HashMap<Uuid, P>>>,
}

#[derive(thiserror::Error, Debug)]
#[error("Infalible error")]
struct MemStorageError;

impl ProjectionStorageError for MemStorageError {}

impl<P: Sized + Projection + Clone> ProjectionStore for MemProjectionStore<P> {
    type Error = MemStorageError;
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

struct MemProjector<P: Projection>(MemProjectionStore<P>);

// impl<P> Projector for MemProjector<P>
// where
//     P: Projection + Clone,
// {
//     type Error = ProjectorError;
//     type Projection = P;
//     type Store = MemProjectionStore<Self::Projection>;
//     fn get_store(&self) -> &Self::Store {
//         &self.0
//     }
// }
