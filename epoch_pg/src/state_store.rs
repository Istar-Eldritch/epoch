use async_trait::async_trait;
use epoch_core::prelude::StateStoreBackend;
use sqlx::PgExecutor;
use sqlx::{FromRow, PgPool, postgres::PgRow};
use std::marker::PhantomData;
use uuid::Uuid;

/// State store implementation for postgres
#[derive(Clone)]
pub struct PgStateStore<T> {
    _phantom: PhantomData<T>,
    pg: PgPool,
}

impl<T> PgStateStore<T> {
    /// Creates a new `PgStateStore` instance.
    pub fn new(pool: PgPool) -> Self {
        Self {
            _phantom: PhantomData,
            pg: pool,
        }
    }
}

/// Trait definnig the required behavior of a PG state;
#[async_trait]
pub trait PgState: Send + Sync + for<'r> FromRow<'r, PgRow> + Unpin {
    /// Retrieve the entity from the storage using its id
    async fn find_by_id<'a, T>(id: Uuid, executor: T) -> Result<Option<Self>, sqlx::Error>
    where
        T: PgExecutor<'a>;

    /// The upsert operation to save this entity in postgres
    async fn upsert<'a, T>(id: Uuid, state: &'a Self, executor: T) -> Result<(), sqlx::Error>
    where
        T: PgExecutor<'a>;

    /// The upsert operation to save this entity in postgres
    async fn delete<'a, T>(id: Uuid, executor: T) -> Result<(), sqlx::Error>
    where
        T: PgExecutor<'a>;
}

#[async_trait]
impl<T> StateStoreBackend<T> for PgStateStore<T>
where
    T: PgState,
{
    type Error = sqlx::Error;
    /// Retrieves the state for a given ID.
    async fn get_state(&self, id: Uuid) -> Result<Option<T>, Self::Error> {
        let mut conn = self.pg.acquire().await?;
        Ok(T::find_by_id(id, &mut *conn).await?)
    }
    /// Persists the state for a given ID.
    async fn persist_state(&mut self, id: Uuid, state: T) -> Result<(), Self::Error> {
        let mut conn = self.pg.acquire().await?;
        T::upsert(id, &state, &mut *conn).await?;
        Ok(())
    }
    /// Deletes the state for a given ID.
    async fn delete_state(&mut self, id: Uuid) -> Result<(), Self::Error> {
        let mut conn = self.pg.acquire().await?;
        T::delete(id, &mut *conn).await?;
        Ok(())
    }
}
