use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use std::time::Duration;

/// Loads `epoch_pg/.env` if it exists.
///
/// Uses `CARGO_MANIFEST_DIR` so the lookup is always relative to the crate
/// root, regardless of where `cargo test` is invoked from.
fn load_env() {
    let env_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".env");
    dotenvy::from_path(env_path).ok();
}

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
    let exists: bool = sqlx::query("SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)")
        .bind(db_name)
        .fetch_one(&pool)
        .await?
        .get(0);

    // Create database if it doesn't exist
    if !exists {
        // Note: Can't use parameterized query for database name
        let create_db_query = format!("CREATE DATABASE \"{}\"", db_name);
        sqlx::query(&create_db_query).execute(&pool).await?;
        println!("Created test database: {}", db_name);
    }

    pool.close().await;
    Ok(())
}

/// Returns the test database URL.
///
/// Resolution order:
/// 1. `DATABASE_URL` environment variable (already set in the process)
/// 2. `epoch_pg/.env` file (loaded via `dotenvy`)
/// 3. Hard-coded fallback: `postgres://postgres:postgres@localhost:5432/epoch_pg_test`
///
/// Copy `.env.example` to `.env` and adjust `POSTGRES_PORT` / `DATABASE_URL`
/// to avoid conflicts with other Postgres instances on your machine.
pub fn database_url() -> String {
    load_env();
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost:5432/epoch_pg_test".to_string())
}

/// Tries to get a connection pool to the test database.
///
/// Returns `None` when Postgres is not reachable, allowing tests to skip
/// gracefully in environments without a database.
///
/// # Strict mode (CI)
///
/// Skipping silently can mask a misconfigured CI pipeline: every integration
/// test "passes" without ever touching the database. Set `EPOCH_REQUIRE_DB=1`
/// to turn an unreachable database into a hard test failure instead of a skip.
#[allow(dead_code)]
pub async fn try_get_pg_pool() -> Option<PgPool> {
    try_get_pg_pool_at(&database_url()).await
}

/// Like [`try_get_pg_pool`], but connects to a dedicated database named
/// `db_name` on the same Postgres server as the configured test database.
///
/// Use this for **destructive** test suites — e.g. the migration tests, which
/// `DROP` every epoch table to exercise migrations from scratch.
/// `cargo test --workspace` runs test binaries as parallel processes, and
/// `#[serial]` only serializes tests *within* one binary; a destructive suite
/// sharing the default test database races every other integration binary.
#[allow(dead_code)]
pub async fn try_get_pg_pool_for_db(db_name: &str) -> Option<PgPool> {
    let mut url = match url::Url::parse(&database_url()) {
        Ok(url) => url,
        Err(e) => {
            eprintln!("Skipping test: invalid DATABASE_URL ({e})");
            return None;
        }
    };
    url.set_path(&format!("/{db_name}"));
    try_get_pg_pool_at(url.as_str()).await
}

async fn try_get_pg_pool_at(database_url: &str) -> Option<PgPool> {
    if let Err(e) = ensure_test_database_exists(database_url).await {
        eprintln!(
            "Warning: Could not ensure test database exists: {}. Attempting to connect anyway...",
            e
        );
    }

    let pool = PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(Duration::from_secs(5))
        .connect(database_url)
        .await;

    match pool {
        Ok(pool) => Some(pool),
        Err(e) => {
            if std::env::var("EPOCH_REQUIRE_DB").is_ok_and(|v| v == "1") {
                panic!(
                    "EPOCH_REQUIRE_DB=1 but Postgres is unreachable at {database_url}: {e}. \
                     Integration tests must not be skipped in this environment."
                );
            }
            eprintln!(
                "Skipping test: Postgres unavailable ({e}). \
                 Set DATABASE_URL (or epoch_pg/.env) to reach a running instance, \
                 or EPOCH_REQUIRE_DB=1 to make this a failure."
            );
            None
        }
    }
}

/// Gets a connection pool to the test database.
///
/// Automatically creates the database if it doesn't exist.
#[allow(dead_code)]
pub async fn get_pg_pool() -> PgPool {
    let database_url = database_url();

    // Ensure the database exists before trying to connect
    if let Err(e) = ensure_test_database_exists(&database_url).await {
        eprintln!(
            "Warning: Could not ensure test database exists: {}. Attempting to connect anyway...",
            e
        );
    }

    PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(Duration::from_secs(30))
        .connect(&database_url)
        .await
        .expect("Failed to create Postgres pool")
}
