//! This module defines traits for `Projection`
//! A `Projection` is a read-model built from a stream of events.

use std::pin::Pin;

use uuid::Uuid;

use crate::{
    event::{Event, EventData},
    prelude::EventStoreBackend,
};

/// A trait that defines the behavior of a projection.
/// A projection is a read-model that is built from a stream of events.
pub trait Projection: Send + Sync {
    /// The type of events the projection can handle
    type EventType: EventData + Send + Sync;
    /// Updates the projection with a single event.
    fn apply(
        &mut self,
        event: &Event<Self::EventType>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Returns the id of this projection
    fn id(&self) -> Uuid;

    /// Returns the version of this projection
    fn version(&self) -> u64;
}

/// Used to orchestrate the projection lifecycle
pub trait ProjectionRepository {
    /// The projection used by this repository
    type Projection: Projection;
    /// Applies the event to a projection.
    fn apply<'a>(
        &'a self,
        event: &Event<<Self::Projection as Projection>::EventType>,
    ) -> Pin<
        Box<dyn Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send + 'a>,
    > {
        let id = self.get_id_for_event(event);
        let backend = self.store_backend();
        let event = event.clone();
        Box::pin(async move {
            let entity = backend.find_by_id(&id).await;
            entity.and_then(|e| e.map(|mut e| e.apply(&event)).transpose())?;
            Ok(())
        })
    }

    /// Returns the id of the projection affected by an event
    fn get_id_for_event(&self, event: &Event<<Self::Projection as Projection>::EventType>) -> Uuid;

    /// Returns the backend store for the projection state
    fn store_backend(
        &self,
    ) -> Box<dyn ProjectionStoreBackend<Projection = Self::Projection> + Send>;
}

/// A trait that defines persistance and retrieval methods for a projection
pub trait ProjectionStoreBackend {
    /// The state of the projection to be persisted
    type Projection: Projection;
    /// Persists the projection to the underlying store
    fn persist(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send>>;

    /// Retrieves a projection by its id
    fn find_by_id(
        &self,
        id: &Uuid,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Option<Self::Projection>,
                        Box<dyn std::error::Error + Send + Sync>,
                    >,
                > + Send,
        >,
    >;

    /// Lists projections from the store
    fn list(
        &self,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Vec<Self::Projection>,
                        Box<dyn std::error::Error + Send + Sync>,
                    >,
                > + Send,
        >,
    >;
}
