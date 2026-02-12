//! Database migration system for epoch_pg.
//!
//! This module provides a simple, embedded migration system for managing
//! PostgreSQL schema changes. Migrations are versioned, checksummed, and
//! tracked in the database.
//!
//! # Usage
//!
//! ```rust,ignore
//! use epoch_pg::migrations::Migrator;
//! use sqlx::PgPool;
//!
//! let pool = PgPool::connect("postgres://...").await?;
//! let migrator = Migrator::new(pool);
//!
//! // Run all pending migrations
//! let applied = migrator.run().await?;
//! println!("Applied {} migrations", applied);
//!
//! // Check current version
//! println!("Current version: {}", migrator.current_version().await?);
//! ```
//!
//! # Adding New Migrations
//!
//! 1. Create a new file `mXXX_description.rs` in this directory
//! 2. Implement the `Migration` trait
//! 3. Add the migration to the `MIGRATIONS` array in this file

mod m001_create_events_table;
mod m002_add_global_sequence;
mod m003_create_event_bus_infrastructure;
mod m004_rename_tables_with_epoch_prefix;
mod m005_add_checkpoint_sequence_index;
mod m006_add_dlq_resolution_columns;
mod m007_add_causation_columns;

use m001_create_events_table::CreateEventsTable;
use m002_add_global_sequence::AddGlobalSequence;
use m003_create_event_bus_infrastructure::CreateEventBusInfrastructure;
use m004_rename_tables_with_epoch_prefix::RenameTablesWithEpochPrefix;
use m005_add_checkpoint_sequence_index::AddCheckpointSequenceIndex;
use m006_add_dlq_resolution_columns::AddDlqResolutionColumns;
use m007_add_causation_columns::AddCausationColumns;

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row, Transaction};

/// All migrations in order. Add new migrations to the end.
///
/// # Design Note: No Rollback Support
///
/// This migration system intentionally does not support `down()` migrations.
/// For event-sourced systems, rollbacks are problematic because:
///
/// 1. **Event immutability**: Events are the source of truth and cannot be "un-written"
/// 2. **Projection rebuilds**: Projections can be rebuilt from events, making schema
///    rollbacks less necessary
/// 3. **Data loss risk**: Rollbacks often lead to data loss in ways that are hard to recover
/// 4. **Production safety**: Forward-only migrations encourage careful planning and testing
///
/// For development/testing, use a fresh database or truncate tables directly.
/// For production issues, prefer forward migrations that fix problems rather than rollbacks.
const MIGRATIONS: &[&dyn Migration] = &[
    &CreateEventsTable,
    &AddGlobalSequence,
    &CreateEventBusInfrastructure,
    &RenameTablesWithEpochPrefix,
    &AddCheckpointSequenceIndex,
    &AddDlqResolutionColumns,
    &AddCausationColumns,
];

/// Errors that can occur during migration operations.
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    /// A database error occurred.
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    /// A migration's checksum doesn't match what was previously applied.
    #[error("Migration {version} ({name}) checksum mismatch: expected {expected}, found {found}")]
    ChecksumMismatch {
        /// The version of the migration with mismatched checksum.
        version: i64,
        /// The name of the migration.
        name: String,
        /// The checksum that was expected (from the database).
        expected: String,
        /// The checksum that was found (from the code).
        found: String,
    },

    /// A migration failed to execute.
    #[error("Migration {version} ({name}) failed: {reason}")]
    MigrationFailed {
        /// The version of the migration that failed.
        version: i64,
        /// The name of the migration.
        name: String,
        /// The reason for the failure.
        reason: String,
    },
}

/// Represents a single database migration.
///
/// Migrations are defined as structs implementing this trait, allowing
/// both simple SQL execution and complex Rust logic when needed.
///
/// # Example
///
/// ```rust,ignore
/// use async_trait::async_trait;
/// use epoch_pg::migrations::{Migration, MigrationError};
/// use sqlx::{Postgres, Transaction};
///
/// pub struct MyMigration;
///
/// #[async_trait]
/// impl Migration for MyMigration {
///     fn version(&self) -> i64 { 42 }
///     
///     fn name(&self) -> &'static str { "my_migration" }
///     
///     async fn up<'a>(
///         &self,
///         tx: &mut Transaction<'a, Postgres>,
///     ) -> Result<(), MigrationError> {
///         sqlx::query("CREATE TABLE my_table (id INT)")
///             .execute(&mut **tx)
///             .await?;
///         Ok(())
///     }
/// }
/// ```
#[async_trait]
pub trait Migration: Send + Sync {
    /// Unique version number (e.g., 1, 2, 3...).
    ///
    /// Must be unique across all migrations and should be sequential.
    fn version(&self) -> i64;

    /// Human-readable name (e.g., "create_events_table").
    fn name(&self) -> &'static str;

    /// Execute the migration within the provided transaction.
    ///
    /// The transaction is managed by the Migrator - do not commit or rollback.
    async fn up<'a>(&self, tx: &mut Transaction<'a, Postgres>) -> Result<(), MigrationError>;

    /// Returns the checksum of this migration for tamper detection.
    ///
    /// Default implementation computes SHA-256 of version + name.
    /// Override if you want to include SQL content in the checksum.
    fn checksum(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.version().to_le_bytes());
        hasher.update(self.name().as_bytes());
        format!("{:x}", hasher.finalize())
    }
}

/// Record of a migration that has been applied.
#[derive(Debug, Clone)]
pub struct AppliedMigration {
    /// The version number of the migration.
    pub version: i64,
    /// The human-readable name of the migration.
    pub name: String,
    /// When the migration was applied.
    pub applied_at: chrono::DateTime<chrono::Utc>,
    /// The checksum recorded when the migration was applied.
    pub checksum: String,
}

/// Handles database migrations for epoch_pg.
///
/// The migrator tracks applied migrations in the `_epoch_migrations` table
/// and ensures migrations are applied in order, exactly once.
#[derive(Debug, Clone)]
pub struct Migrator {
    pool: PgPool,
}

impl Migrator {
    /// Creates a new migrator with the given connection pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Ensures the migration tracking table exists.
    async fn ensure_tracking_table(&self) -> Result<(), MigrationError> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS _epoch_migrations (
                version BIGINT PRIMARY KEY,
                name VARCHAR(255) NOT NULL,
                applied_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                checksum VARCHAR(64) NOT NULL
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Runs all pending migrations.
    ///
    /// Each migration runs in its own transaction. If a migration fails,
    /// that transaction is rolled back but previously applied migrations
    /// remain committed.
    ///
    /// Returns the number of migrations applied.
    pub async fn run(&self) -> Result<usize, MigrationError> {
        self.ensure_tracking_table().await?;

        // Check for checksum mismatches first
        let applied = self.applied().await?;
        for applied_migration in &applied {
            if let Some(migration) = MIGRATIONS
                .iter()
                .find(|m| m.version() == applied_migration.version)
            {
                let current_checksum = migration.checksum();
                if current_checksum != applied_migration.checksum {
                    return Err(MigrationError::ChecksumMismatch {
                        version: applied_migration.version,
                        name: applied_migration.name.clone(),
                        expected: applied_migration.checksum.clone(),
                        found: current_checksum,
                    });
                }
            }
        }

        let applied_versions: std::collections::HashSet<i64> =
            applied.iter().map(|m| m.version).collect();

        let mut count = 0;
        for migration in MIGRATIONS {
            if applied_versions.contains(&migration.version()) {
                log::debug!(
                    "Skipping migration {} ({}): already applied",
                    migration.version(),
                    migration.name()
                );
                continue;
            }

            log::info!(
                "Running migration {} ({})...",
                migration.version(),
                migration.name()
            );

            let mut tx = self.pool.begin().await?;

            // Run the migration
            migration.up(&mut tx).await.map_err(|e| match e {
                MigrationError::Database(db_err) => MigrationError::MigrationFailed {
                    version: migration.version(),
                    name: migration.name().to_string(),
                    reason: db_err.to_string(),
                },
                other => other,
            })?;

            // Record the migration
            sqlx::query(
                r#"
                INSERT INTO _epoch_migrations (version, name, checksum)
                VALUES ($1, $2, $3)
                "#,
            )
            .bind(migration.version())
            .bind(migration.name())
            .bind(migration.checksum())
            .execute(&mut *tx)
            .await?;

            tx.commit().await?;

            log::info!(
                "Migration {} ({}) applied successfully",
                migration.version(),
                migration.name()
            );
            count += 1;
        }

        Ok(count)
    }

    /// Returns the current migration version (0 if no migrations applied).
    pub async fn current_version(&self) -> Result<i64, MigrationError> {
        self.ensure_tracking_table().await?;

        let row: Option<(i64,)> = sqlx::query_as(
            r#"
            SELECT version FROM _epoch_migrations
            ORDER BY version DESC
            LIMIT 1
            "#,
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(|(v,)| v).unwrap_or(0))
    }

    /// Returns list of all pending migrations.
    pub async fn pending(&self) -> Result<Vec<&'static dyn Migration>, MigrationError> {
        self.ensure_tracking_table().await?;

        let applied_versions: std::collections::HashSet<i64> =
            self.applied().await?.iter().map(|m| m.version).collect();

        Ok(MIGRATIONS
            .iter()
            .filter(|m| !applied_versions.contains(&m.version()))
            .copied()
            .collect())
    }

    /// Returns list of all applied migrations.
    pub async fn applied(&self) -> Result<Vec<AppliedMigration>, MigrationError> {
        self.ensure_tracking_table().await?;

        let rows = sqlx::query(
            r#"
            SELECT version, name, applied_at, checksum
            FROM _epoch_migrations
            ORDER BY version ASC
            "#,
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|row| AppliedMigration {
                version: row.get("version"),
                name: row.get("name"),
                applied_at: row.get("applied_at"),
                checksum: row.get("checksum"),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_checksum_is_deterministic() {
        let checksum1 = CreateEventsTable.checksum();
        let checksum2 = CreateEventsTable.checksum();
        assert_eq!(checksum1, checksum2);
    }

    #[test]
    fn different_migrations_have_different_checksums() {
        let checksum1 = CreateEventsTable.checksum();
        let checksum2 = AddGlobalSequence.checksum();
        assert_ne!(checksum1, checksum2);
    }

    #[test]
    fn migrations_are_in_order() {
        let mut prev_version = 0;
        for migration in MIGRATIONS {
            assert!(
                migration.version() > prev_version,
                "Migration {} should have version > {}",
                migration.name(),
                prev_version
            );
            prev_version = migration.version();
        }
    }

    #[test]
    fn all_migrations_have_unique_versions() {
        let versions: Vec<i64> = MIGRATIONS.iter().map(|m| m.version()).collect();
        let unique: std::collections::HashSet<i64> = versions.iter().copied().collect();
        assert_eq!(
            versions.len(),
            unique.len(),
            "Migration versions must be unique"
        );
    }

    #[test]
    fn all_migrations_have_unique_names() {
        let names: Vec<&str> = MIGRATIONS.iter().map(|m| m.name()).collect();
        let unique: std::collections::HashSet<&str> = names.iter().copied().collect();
        assert_eq!(names.len(), unique.len(), "Migration names must be unique");
    }
}
