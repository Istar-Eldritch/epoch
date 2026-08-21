use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use std::sync::{Mutex, Once, OnceLock};
use std::time::Duration;

static CAPTURED_LOGS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
static LOGGER_INIT: Once = Once::new();
static CAPTURE_LOGGER: CaptureLogger = CaptureLogger;

fn captured_logs() -> &'static Mutex<Vec<String>> {
    CAPTURED_LOGS.get_or_init(|| Mutex::new(Vec::new()))
}

struct CaptureLogger;

impl log::Log for CaptureLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        // Only retain WARN/ERROR so the buffer stays small and matches the
        // default (silent) env_logger behaviour for lower levels.
        if record.level() <= log::Level::Warn {
            let line = format!("[{}] {}", record.level(), record.args());
            // Also print, so a failing test still surfaces the WARN/ERROR in
            // `cargo test`'s captured-output-on-failure, matching what the
            // previous env_logger-based setup gave for free.
            eprintln!("{line}");
            captured_logs().lock().unwrap().push(line);
        }
    }

    fn flush(&self) {}
}

/// Installs a process-global logger that captures WARN/ERROR records (and
/// still prints them, so a failing test surfaces them) so tests can assert a
/// specific warning was emitted. Idempotent and safe to call from every test;
/// replaces the previous `env_logger` init in this test binary.
#[allow(dead_code)]
pub fn init_test_logger() {
    LOGGER_INIT.call_once(|| {
        // If another logger is already installed process-wide for this binary
        // (unusual, but possible), leave it in place: every
        // `captured_logs_contain_since` call below then sees an always-empty
        // buffer, so a test relying on it fails loudly instead of silently
        // passing against no captured output.
        let _ = log::set_logger(&CAPTURE_LOGGER);
        log::set_max_level(log::LevelFilter::Trace);
    });
}

/// Current length of the captured-log buffer. Snapshot this **before** the
/// action under test, then pass it to [`captured_logs_contain_since`].
///
/// The buffer is process-global and shared across every test in this binary
/// running concurrently (only `#[serial]` tests are mutually exclusive with
/// each other, not with non-serial ones), so a destructive "clear before,
/// assert after" API would race: one test's clear can drop another
/// concurrently-running test's own log line before it gets to assert on it.
/// Snapshotting a start index and only scanning the suffix avoids that.
#[allow(dead_code)]
pub fn captured_logs_len() -> usize {
    captured_logs().lock().unwrap().len()
}

/// Returns true if any captured WARN/ERROR record appended at or after
/// `start` (see [`captured_logs_len`]) contains `needle`.
#[allow(dead_code)]
pub fn captured_logs_contain_since(start: usize, needle: &str) -> bool {
    let logs = captured_logs().lock().unwrap();
    let start = start.min(logs.len());
    logs[start..].iter().any(|line| line.contains(needle))
}

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
        // Test binaries share one database, so a test holding an open
        // transaction on epoch_events can block a sibling's TRUNCATE, whose
        // pending ACCESS EXCLUSIVE then queues ahead of every later reader and
        // writer. The holder is idle in transaction rather than waiting, so
        // Postgres sees no lock cycle and nothing ever breaks it: a run wedged
        // for over five hours this way. These turn that permanent wedge into a
        // fast, loud failure.
        //
        // 30s, not 15s: legitimate holds in this suite reach ~13s (the sentinel
        // transaction spans two 6s poll loops), so a tighter bound would trade
        // the wedge for a flake under load.
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                // Separate statements: sqlx::query uses the extended protocol,
                // which rejects multiple `;`-separated commands.
                sqlx::query("SET lock_timeout = '30s'")
                    .execute(&mut *conn)
                    .await?;
                sqlx::query("SET idle_in_transaction_session_timeout = '60s'")
                    .execute(&mut *conn)
                    .await?;
                Ok(())
            })
        })
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

/// Truncates all epoch tables and resets the global-sequence counter.
///
/// Call this at the start of every integration test that uses the shared
/// test database, after running migrations. This guarantees each test begins
/// from empty state, preventing accumulated events and sequence gaps from
/// prior runs inflating catch-up time or triggering spurious gap-timeout waits.
#[allow(dead_code)]
pub async fn truncate_epoch_tables(pool: &PgPool) {
    sqlx::query(
        "TRUNCATE epoch_events, epoch_event_bus_checkpoints, \
         epoch_event_bus_dlq, epoch_event_bus_gap_timeouts, \
         epoch_snapshots RESTART IDENTITY",
    )
    .execute(pool)
    .await
    .expect("Failed to truncate epoch tables");
}
