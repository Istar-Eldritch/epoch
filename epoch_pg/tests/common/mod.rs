use sqlx::{PgPool, postgres::PgPoolOptions, Row};
use std::time::Duration;

/// Ensures the test database exists, creating it if necessary.
///
/// This function:
/// 1. Parses the DATABASE_URL to extract the database name
/// 2. Connects to the 'postgres' maintenance database
/// 3. Checks if the target database exists
/// 4. Creates it if it doesn't exist
///
/// This allows tests to run without manual database setup.
async fn ensure_test_database_exists(database_url: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Parse the database URL to extract connection info and database name
    let url = url::Url::parse(database_url)?;
    let db_name = url.path().trim_start_matches('/');
    
    // If no database name specified, skip creation
    if db_name.is_empty() {
        return Ok(());
    }

    // Build connection to 'postgres' database for administrative operations
    let mut maintenance_url = url.clone();
    maintenance_url.set_path("/postgres");
    
    // Connect to maintenance database
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(maintenance_url.as_str())
        .await?;

    // Check if database exists
    let exists: bool = sqlx::query(
        "SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)"
    )
    .bind(db_name)
    .fetch_one(&pool)
    .await?
    .get(0);

    // Create database if it doesn't exist
    if !exists {
        // Note: Can't use parameterized query for database name
        let create_db_query = format!("CREATE DATABASE \"{}\"", db_name);
        sqlx::query(&create_db_query)
            .execute(&pool)
            .await?;
        println!("Created test database: {}", db_name);
    }

    pool.close().await;
    Ok(())
}

/// Returns the test database URL.
///
/// Defaults to `epoch_pg_test` database to avoid conflicts with other projects.
/// Override with `DATABASE_URL` environment variable.
pub fn database_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost:5432/epoch_pg_test".to_string())
}

/// Gets a connection pool to the test database.
///
/// Automatically creates the database if it doesn't exist.
pub async fn get_pg_pool() -> PgPool {
    let database_url = database_url();
    
    // Ensure the database exists before trying to connect
    if let Err(e) = ensure_test_database_exists(&database_url).await {
        eprintln!("Warning: Could not ensure test database exists: {}. Attempting to connect anyway...", e);
    }
    
    PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(Duration::from_secs(30))
        .connect(&database_url)
        .await
        .expect("Failed to create Postgres pool")
}
