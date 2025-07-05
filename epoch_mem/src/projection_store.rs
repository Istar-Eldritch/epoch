use std::{collections::HashMap, sync::Arc};

use epoch_core::{Projection, ProjectionStore, ProjectionStoreError};
use futures_core::Future;
use tokio::sync::Mutex;
use uuid::Uuid;

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
