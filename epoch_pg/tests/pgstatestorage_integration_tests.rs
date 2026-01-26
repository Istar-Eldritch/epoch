mod common;

use async_trait::async_trait;
use epoch_core::prelude::StateStoreBackend;
use epoch_pg::state_store::{PgState, PgStateStore};
use serde::{Deserialize, Serialize};
use serial_test::serial;
use sqlx::{FromRow, PgExecutor, PgPool};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, FromRow)]
struct TestState {
    id: Uuid,
    value: String,
}

#[async_trait]
impl PgState for TestState {
    async fn find_by_id<'a, T>(id: Uuid, executor: T) -> Result<Option<Self>, sqlx::Error>
    where
        T: PgExecutor<'a>,
    {
        sqlx::query_as("select * from test_states where id = $1")
            .bind(id)
            .fetch_optional(executor)
            .await
    }

    async fn find_by_id_for_update<'a, T>(
        id: Uuid,
        executor: T,
    ) -> Result<Option<Self>, sqlx::Error>
    where
        T: PgExecutor<'a>,
    {
        sqlx::query_as("select * from test_states where id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(executor)
            .await
    }

    async fn upsert<'a, T>(id: Uuid, state: &'a Self, executor: T) -> Result<(), sqlx::Error>
    where
        T: PgExecutor<'a>,
    {
        sqlx::query(
        "INSERT INTO test_states (id, value) VALUES ($1, $2) ON CONFLICT (id) DO UPDATE SET value = $2"
            )
            .bind(id)
            .bind(&state.value)
            .execute(executor).await?;
        Ok(())
    }

    async fn delete<'a, T>(id: Uuid, executor: T) -> Result<(), sqlx::Error>
    where
        T: PgExecutor<'a>,
    {
        sqlx::query("delete from test_states where id = $1")
            .bind(id)
            .execute(executor)
            .await?;
        Ok(())
    }
}

async fn setup() -> (PgPool, PgStateStore<TestState>) {
    let pool = common::get_pg_pool().await;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS test_states (id UUID PRIMARY KEY, value TEXT NOT NULL)",
    )
    .execute(&pool)
    .await
    .expect("Failed to create table");
    let state_storage = PgStateStore::new(pool.clone());
    (pool, state_storage)
}

async fn teardown(pool: &PgPool) {
    sqlx::query("DROP TABLE IF EXISTS test_states CASCADE")
        .execute(pool)
        .await
        .expect("Failed to drop table");
}

#[tokio::test]
#[serial]
async fn test_persist_and_get_state() {
    let (pool, mut state_storage) = setup().await;

    let id = Uuid::new_v4();
    let state = TestState {
        id,
        value: "test_value".to_string(),
    };

    state_storage
        .persist_state(id, state.clone())
        .await
        .unwrap();

    let retrieved_state = state_storage.get_state(id).await.unwrap().unwrap();
    assert_eq!(retrieved_state, state);

    teardown(&pool).await;
}

#[tokio::test]
#[serial]
async fn test_update_state() {
    let (pool, mut state_storage) = setup().await;

    let id = Uuid::new_v4();
    let initial_state = TestState {
        id,
        value: "initial_value".to_string(),
    };
    state_storage
        .persist_state(id, initial_state)
        .await
        .unwrap();

    let updated_state = TestState {
        id,
        value: "updated_value".to_string(),
    };
    state_storage
        .persist_state(id, updated_state.clone())
        .await
        .unwrap();

    let retrieved_state = state_storage.get_state(id).await.unwrap().unwrap();
    assert_eq!(retrieved_state, updated_state);

    teardown(&pool).await;
}

#[tokio::test]
#[serial]
async fn test_delete_state() {
    let (pool, mut state_storage) = setup().await;

    let id = Uuid::new_v4();
    let state = TestState {
        id,
        value: "to_be_deleted".to_string(),
    };

    state_storage.persist_state(id, state).await.unwrap();

    let retrieved_state_before_delete = state_storage.get_state(id).await.unwrap();
    assert!(retrieved_state_before_delete.is_some());

    state_storage.delete_state(id).await.unwrap();

    let retrieved_state_after_delete = state_storage.get_state(id).await.unwrap();
    assert!(retrieved_state_after_delete.is_none());

    teardown(&pool).await;
}

#[tokio::test]
#[serial]
async fn test_get_non_existent_state() {
    let (pool, state_storage) = setup().await;

    let id = Uuid::new_v4();
    let retrieved_state = state_storage.get_state(id).await.unwrap();
    assert!(retrieved_state.is_none());

    teardown(&pool).await;
}
