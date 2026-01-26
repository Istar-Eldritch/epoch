use async_trait::async_trait;
use epoch_core::prelude::StateStoreBackend;
use sqlx::PgExecutor;
use sqlx::PgPool;
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

/// Trait defining the required behavior of a PG state.
#[async_trait]
pub trait PgState: Send + Sync + Sized + Unpin {
    /// Retrieve the entity from the storage using its id.
    async fn find_by_id<'a, T>(id: Uuid, executor: T) -> Result<Option<Self>, sqlx::Error>
    where
        T: PgExecutor<'a>;

    /// Retrieve the entity with a row-level lock for update.
    ///
    /// Uses `SELECT ... FOR UPDATE` to prevent concurrent modifications within
    /// the same transaction. This is used by [`TransactionalAggregate`](epoch_core::aggregate::TransactionalAggregate)
    /// to ensure pessimistic locking during command handling.
    ///
    /// Returns `None` if the row doesn't exist (no lock acquired).
    ///
    /// # Lock Behavior
    ///
    /// - **Timeout:** By default, PostgreSQL waits indefinitely for the lock.
    ///   Configure `lock_timeout` on the connection or transaction to set a limit:
    ///   ```sql
    ///   SET lock_timeout = '5s';
    ///   ```
    /// - **Deadlocks:** PostgreSQL automatically detects deadlocks and aborts one
    ///   transaction with an error. To avoid deadlocks, always acquire locks on
    ///   multiple aggregates in a consistent order (e.g., by aggregate ID).
    ///
    /// # Variants
    ///
    /// For non-blocking behavior, implement custom methods using:
    /// - `FOR UPDATE NOWAIT` - Fails immediately if lock is unavailable
    /// - `FOR UPDATE SKIP LOCKED` - Skips locked rows (useful for batch processing)
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut tx = pool.begin().await?;
    /// // This acquires a row-level lock until tx commits/rolls back
    /// let state = MyState::find_by_id_for_update(id, &mut *tx).await?;
    /// // ... modify state ...
    /// tx.commit().await?;
    /// ```
    async fn find_by_id_for_update<'a, T>(
        id: Uuid,
        executor: T,
    ) -> Result<Option<Self>, sqlx::Error>
    where
        T: PgExecutor<'a>;

    /// The upsert operation to save this entity in postgres.
    async fn upsert<'a, T>(id: Uuid, state: &'a Self, executor: T) -> Result<(), sqlx::Error>
    where
        T: PgExecutor<'a>;

    /// Delete the entity from postgres.
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
