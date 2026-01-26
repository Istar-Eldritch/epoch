use sqlx::{PgPool, postgres::PgPoolOptions};
use std::time::Duration;

pub async fn get_pg_pool() -> PgPool {
    let database_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost:5432/epoch_pg".to_string());
    PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(Duration::from_secs(30))
        .connect(&database_url)
        .await
        .expect("Failed to create Postgres pool")
}
