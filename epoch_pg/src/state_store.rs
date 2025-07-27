use async_trait::async_trait;
use epoch_core::prelude::StateStorage;
use sqlx::postgres::PgArguments;
use sqlx::query::Query;
use sqlx::{FromRow, PgPool, postgres::PgRow};
use std::marker::PhantomData;
use uuid::Uuid;

/// State store implementation for postgres
pub struct PgStateStorage<T> {
    _phantom: PhantomData<T>,
    pg: PgPool,
}

impl<T> PgStateStorage<T> {
    /// Creates a new `PgStateStorage` instance.
    pub fn new(pool: PgPool) -> Self {
        Self {
            _phantom: PhantomData,
            pg: pool,
        }
    }
}

/// Trait definnig the required behavior of a PG state;
pub trait PgState: Send + Sync + for<'r> FromRow<'r, PgRow> + Unpin {
    /// The table where this state will be persisted
    const TABLE_NAME: &'static str;
    /// The id of the column used to retrieve the state
    const ID_COLUMN: &'static str;

    /// Creates an upsert query for the state.
    fn upsert_query() -> &'static str;

    /// Binds the state values to the query builder.
    fn bind_upsert_values<'a>(
        state: &'a Self,
        query_builder: Query<'a, sqlx::Postgres, PgArguments>,
    ) -> Query<'a, sqlx::Postgres, PgArguments>;
}

#[async_trait]
impl<T> StateStorage<T> for PgStateStorage<T>
where
    T: PgState,
{
    /// Retrieves the state for a given ID.
    async fn get_state(
        &self,
        id: Uuid,
    ) -> Result<Option<T>, Box<dyn std::error::Error + Send + Sync>> {
        let mut conn = self.pg.acquire().await?;
        let query = format!(
            "SELECT * FROM {} WHERE {} = $1",
            T::TABLE_NAME,
            T::ID_COLUMN
        );
        Ok(sqlx::query_as(&query)
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?)
    }
    /// Persists the state for a given ID.
    async fn persist_state(
        &mut self,
        _id: Uuid,
        state: T,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut conn = self.pg.acquire().await?;
        let query_str = T::upsert_query();
        let mut query_builder = sqlx::QueryBuilder::new(query_str);
        let query = query_builder.build();
        let query = T::bind_upsert_values(&state, query);

        query.execute(&mut *conn).await?;

        Ok(())
    }
    /// Deletes the state for a given ID.
    async fn delete_state(
        &mut self,
        id: Uuid,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut conn = self.pg.acquire().await?;
        let query = format!("DELETE FROM {} WHERE {} = $1", T::TABLE_NAME, T::ID_COLUMN);
        sqlx::query(&query).bind(id).execute(&mut *conn).await?;
        Ok(())
    }
}
