//! State storage definitions

use async_trait::async_trait;
use uuid::Uuid;

/// `StateStorage` is a trait that defines the interface for storing and retrieving
/// the state of a `Projection`.
#[async_trait]
pub trait StateStoreBackend<S> {
    /// Retrieves the state for a given ID.
    async fn get_state(
        &self,
        id: Uuid,
    ) -> Result<Option<S>, Box<dyn std::error::Error + Send + Sync>>;
    /// Persists the state for a given ID.
    async fn persist_state(
        &mut self,
        id: Uuid,
        state: S,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
    /// Deletes the state for a given ID.
    async fn delete_state(
        &mut self,
        id: Uuid,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}
