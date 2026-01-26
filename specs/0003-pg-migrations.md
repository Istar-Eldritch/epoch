# Specification: PostgreSQL Migration Mechanism for epoch_pg

## 1. Problem Statement

The `epoch_pg` crate currently handles schema setup through `initialize()` methods on `PgEventStore` and `PgEventBus`. These methods:

1. Mix table creation with schema evolution logic
2. Use ad-hoc `DO $$ ... $$` blocks to check for existing columns/sequences
3. Are difficult to extend and maintain as the schema evolves
4. Don't provide a clear history of schema changes
5. Require users to call multiple `initialize()` methods in the right order

A proper migration mechanism would:
- Provide versioned, trackable schema changes
- Be idempotent and safe to run multiple times
- Support both fresh installations and upgrades
- Give users control over when and how migrations run
- Keep migration history for auditability

## 2. Proposed Solution

### 2.1 Design Goals

1. **Simple**: No external dependencies (no `refinery`, `sqlx-migrate`, etc.)
2. **Embedded**: Migrations are compiled into the library
3. **Versioned**: Each migration has a unique, sequential version number
4. **Idempotent**: Safe to run multiple times
5. **Unified**: Single entry point replaces multiple `initialize()` calls
6. **Flexible**: Trait-based migrations allow complex Rust logic, not just raw SQL
7. **Introspectable**: Rich API for querying migration state
8. **Auditable**: Checksums and names for tracking and validation

### 2.2 Core Components

#### 2.2.1 Migration Tracking Table

```sql
CREATE TABLE IF NOT EXISTS _epoch_migrations (
    version BIGINT PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    checksum VARCHAR(64) NOT NULL
);
```

### 2.3 Table Naming Convention

All epoch tables are prefixed with `epoch_` to avoid conflicts with user tables:

| Table | Purpose |
|-------|---------|
| `_epoch_migrations` | Migration tracking |
| `epoch_events` | Event store |
| `epoch_event_bus_checkpoints` | Subscriber progress tracking |
| `epoch_event_bus_dlq` | Dead letter queue for failed events |

Related objects (sequences, indexes, functions) also use the `epoch_` prefix.

#### 2.2.2 Migration Trait

```rust
use async_trait::async_trait;
use sqlx::{Postgres, Transaction};

/// Represents a single database migration.
/// 
/// Migrations are defined as structs implementing this trait, allowing
/// both simple SQL execution and complex Rust logic when needed.
#[async_trait]
pub trait Migration: Send + Sync {
    /// Unique version number (e.g., 1, 2, 3...)
    /// Must be unique across all migrations and should be sequential.
    fn version(&self) -> i64;
    
    /// Human-readable name (e.g., "create_events_table")
    fn name(&self) -> &'static str;
    
    /// Execute the migration within the provided transaction.
    /// 
    /// The transaction is managed by the Migrator - do not commit or rollback.
    async fn up<'a>(
        &self,
        tx: &mut Transaction<'a, Postgres>,
    ) -> Result<(), MigrationError>;
    
    /// Returns the checksum of this migration for tamper detection.
    /// 
    /// Default implementation computes SHA-256 of name + version.
    /// Override if you want to include SQL content in the checksum.
    fn checksum(&self) -> String {
        use sha2::{Sha256, Digest};
        let mut hasher = Sha256::new();
        hasher.update(self.version().to_le_bytes());
        hasher.update(self.name().as_bytes());
        format!("{:x}", hasher.finalize())
    }
}
```

#### 2.2.3 Migrator

```rust
use sqlx::PgPool;

/// Handles database migrations for epoch_pg.
pub struct Migrator {
    pool: PgPool,
}

impl Migrator {
    /// Creates a new migrator with the given connection pool.
    pub fn new(pool: PgPool) -> Self;
    
    /// Runs all pending migrations.
    /// 
    /// Each migration runs in its own transaction. If a migration fails,
    /// that transaction is rolled back but previously applied migrations
    /// remain committed.
    /// 
    /// Returns the number of migrations applied.
    pub async fn run(&self) -> Result<usize, MigrationError>;
    
    /// Returns the current migration version (0 if no migrations applied).
    pub async fn current_version(&self) -> Result<i64, MigrationError>;
    
    /// Returns list of all pending migrations.
    pub async fn pending(&self) -> Result<Vec<&'static dyn Migration>, MigrationError>;
    
    /// Returns list of all applied migrations.
    pub async fn applied(&self) -> Result<Vec<AppliedMigration>, MigrationError>;
}

/// Record of a migration that has been applied.
#[derive(Debug, Clone)]
pub struct AppliedMigration {
    pub version: i64,
    pub name: String,
    pub applied_at: chrono::DateTime<chrono::Utc>,
    pub checksum: String,
}
```

#### 2.2.4 Migration Errors

```rust
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    
    #[error("Migration {version} ({name}) checksum mismatch: expected {expected}, found {found}")]
    ChecksumMismatch {
        version: i64,
        name: String,
        expected: String,
        found: String,
    },
    
    #[error("Migration {version} ({name}) failed: {reason}")]
    MigrationFailed {
        version: i64,
        name: String,
        reason: String,
    },
}
```

### 2.3 File Structure

Each migration lives in its own file for clarity and easier code review:

```
epoch_pg/
├── src/
│   ├── lib.rs
│   ├── migrations/
│   │   ├── mod.rs              # Migrator, Migration trait, errors, MIGRATIONS registry
│   │   ├── m001_create_events_table.rs
│   │   ├── m002_add_global_sequence.rs
│   │   ├── m003_create_event_bus_infrastructure.rs
│   │   └── m004_rename_tables_with_epoch_prefix.rs
│   ├── event_store.rs
│   ├── event_bus.rs
│   └── state_store.rs
```

### 2.4 Migrations

The migration strategy handles both fresh installations and upgrades from existing production databases:

1. **Migrations 1-3**: Create tables with **legacy names** (e.g., `events`, `event_bus_checkpoints`)
   - Uses `IF NOT EXISTS` so existing tables are preserved
   - Allows the migrator to work with pre-existing production databases
   
2. **Migration 4**: Renames all tables to use the `epoch_` prefix
   - Uses `IF EXISTS` checks to handle both fresh installs and upgrades
   - Renames tables, sequences, indexes, constraints, and functions

#### Initial Migrations

#### m001_create_events_table.rs

```rust
use async_trait::async_trait;
use sqlx::{Postgres, Transaction};
use super::{Migration, MigrationError};

pub struct CreateEventsTable;

#[async_trait]
impl Migration for CreateEventsTable {
    fn version(&self) -> i64 { 1 }
    
    fn name(&self) -> &'static str { "create_events_table" }
    
    async fn up<'a>(&self, tx: &mut Transaction<'a, Postgres>) -> Result<(), MigrationError> {
        // Uses legacy name for backwards compatibility; migration 4 will rename
        sqlx::query(r#"
            CREATE TABLE IF NOT EXISTS events (
                id UUID PRIMARY KEY,
                stream_id UUID NOT NULL,
                stream_version BIGINT NOT NULL,
                event_type VARCHAR(255) NOT NULL,
                data JSONB,
                created_at TIMESTAMPTZ NOT NULL,
                actor_id UUID,
                purger_id UUID,
                purged_at TIMESTAMPTZ,
                UNIQUE (stream_id, stream_version)
            )
        "#)
        .execute(&mut **tx)
        .await?;
        
        Ok(())
    }
}
```

#### m002_add_global_sequence.rs

```rust
use async_trait::async_trait;
use sqlx::{Postgres, Transaction};
use super::{Migration, MigrationError};

pub struct AddGlobalSequence;

#[async_trait]
impl Migration for AddGlobalSequence {
    fn version(&self) -> i64 { 2 }
    
    fn name(&self) -> &'static str { "add_global_sequence" }
    
    async fn up<'a>(&self, tx: &mut Transaction<'a, Postgres>) -> Result<(), MigrationError> {
        // Uses legacy names for backwards compatibility; migration 4 will rename
        sqlx::query(r#"
            CREATE SEQUENCE IF NOT EXISTS events_global_sequence_seq
        "#)
        .execute(&mut **tx)
        .await?;

        sqlx::query(r#"
            ALTER TABLE events 
            ADD COLUMN IF NOT EXISTS global_sequence BIGINT 
            DEFAULT nextval('events_global_sequence_seq')
        "#)
        .execute(&mut **tx)
        .await?;

        // ... backfill and index creation ...
        Ok(())
    }
}
```

#### m003_create_event_bus_infrastructure.rs

```rust
use async_trait::async_trait;
use sqlx::{Postgres, Transaction};
use super::{Migration, MigrationError};

pub struct CreateEventBusInfrastructure;

#[async_trait]
impl Migration for CreateEventBusInfrastructure {
    fn version(&self) -> i64 { 3 }
    
    fn name(&self) -> &'static str { "create_event_bus_infrastructure" }
    
    async fn up<'a>(&self, tx: &mut Transaction<'a, Postgres>) -> Result<(), MigrationError> {
        // Uses legacy names for backwards compatibility; migration 4 will rename
        sqlx::query(r#"
            CREATE OR REPLACE FUNCTION epoch_pg_notify_event()
            RETURNS TRIGGER AS $$
            BEGIN
                PERFORM pg_notify(
                    TG_ARGV[0],
                    json_build_object(
                        'id', NEW.id,
                        'stream_id', NEW.stream_id,
                        'stream_version', NEW.stream_version,
                        'event_type', NEW.event_type,
                        'actor_id', NEW.actor_id,
                        'purger_id', NEW.purger_id,
                        'data', NEW.data,
                        'created_at', NEW.created_at,
                        'purged_at', NEW.purged_at,
                        'global_sequence', NEW.global_sequence
                    )::text
                );
                RETURN NEW;
            END;
            $$ LANGUAGE plpgsql
        "#)
        .execute(&mut **tx)
        .await?;

        // Checkpoint table (legacy name)
        sqlx::query(r#"
            CREATE TABLE IF NOT EXISTS event_bus_checkpoints (
                subscriber_id VARCHAR(255) PRIMARY KEY,
                last_global_sequence BIGINT NOT NULL DEFAULT 0,
                last_event_id UUID,
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )
        "#)
        .execute(&mut **tx)
        .await?;

        // Dead letter queue (legacy name)
        sqlx::query(r#"
            CREATE TABLE IF NOT EXISTS event_bus_dlq (
                id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                subscriber_id VARCHAR(255) NOT NULL,
                event_id UUID NOT NULL,
                global_sequence BIGINT NOT NULL,
                error_message TEXT,
                retry_count INT NOT NULL DEFAULT 0,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                last_retry_at TIMESTAMPTZ,
                CONSTRAINT unique_subscriber_event UNIQUE (subscriber_id, event_id)
            )
        "#)
        .execute(&mut **tx)
        .await?;

        sqlx::query(r#"
            CREATE INDEX IF NOT EXISTS idx_dlq_subscriber 
            ON event_bus_dlq(subscriber_id)
        "#)
        .execute(&mut **tx)
        .await?;

        sqlx::query(r#"
            CREATE INDEX IF NOT EXISTS idx_dlq_created_at 
            ON event_bus_dlq(created_at)
        "#)
        .execute(&mut **tx)
        .await?;

        Ok(())
    }
}
```

#### m004_rename_tables_with_epoch_prefix.rs

```rust
use async_trait::async_trait;
use sqlx::{Postgres, Transaction};
use super::{Migration, MigrationError};

pub struct RenameTablesWithEpochPrefix;

#[async_trait]
impl Migration for RenameTablesWithEpochPrefix {
    fn version(&self) -> i64 { 4 }
    
    fn name(&self) -> &'static str { "rename_tables_with_epoch_prefix" }
    
    async fn up<'a>(&self, tx: &mut Transaction<'a, Postgres>) -> Result<(), MigrationError> {
        // Rename tables (with IF EXISTS checks for idempotency)
        // events -> epoch_events
        // event_bus_checkpoints -> epoch_event_bus_checkpoints
        // event_bus_dlq -> epoch_event_bus_dlq
        
        // Also renames: sequences, indexes, constraints, functions
        // See full implementation in source code
        Ok(())
    }
}
```

### 2.5 Migration Registry

In `migrations/mod.rs`:

```rust
mod m001_create_events_table;
mod m002_add_global_sequence;
mod m003_create_event_bus_infrastructure;
mod m004_rename_tables_with_epoch_prefix;

use m001_create_events_table::CreateEventsTable;
use m002_add_global_sequence::AddGlobalSequence;
use m003_create_event_bus_infrastructure::CreateEventBusInfrastructure;
use m004_rename_tables_with_epoch_prefix::RenameTablesWithEpochPrefix;

/// All migrations in order. Add new migrations to the end.
const MIGRATIONS: &[&dyn Migration] = &[
    &CreateEventsTable,
    &AddGlobalSequence,
    &CreateEventBusInfrastructure,
    &RenameTablesWithEpochPrefix,
];
```

### 2.6 Usage

```rust
use epoch_pg::{Migrator, PgEventStore, PgEventBus};
use sqlx::PgPool;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let pool = PgPool::connect("postgres://...").await?;
    
    // Run migrations (replaces initialize() calls)
    let migrator = Migrator::new(pool.clone());
    let applied = migrator.run().await?;
    println!("Applied {} migrations", applied);
    
    // Check migration status
    println!("Current version: {}", migrator.current_version().await?);
    println!("Pending: {:?}", migrator.pending().await?.len());
    
    // Now create stores as normal
    let event_bus = PgEventBus::new(pool.clone(), "my_channel");
    let event_store = PgEventStore::new(pool.clone(), event_bus.clone());
    
    // Set up the channel-specific trigger and start the listener
    event_bus.setup_trigger().await?;
    event_bus.start_listener().await?;
    
    Ok(())
}
```

### 2.7 Backward Compatibility

1. The `initialize()` methods on `PgEventStore` and `PgEventBus` will be marked as `#[deprecated]`
2. They will internally call the migrator for a smooth transition
3. Existing databases will work - migrations check for existing objects with `IF NOT EXISTS`

### 2.8 Trigger Setup Consideration

The `PgEventBus` trigger is channel-specific (uses the channel name). This remains separate from migrations:

- Trigger creation stays in a `setup_trigger()` method on `PgEventBus`
- Migrations only handle schema, not channel-specific config
- **Rationale**: Cleaner separation of concerns

## 3. Files to Modify

| File | Changes |
|------|---------|
| `epoch_pg/src/lib.rs` | Add `pub mod migrations;` export |
| `epoch_pg/src/migrations/mod.rs` | New: `Migrator`, `Migration` trait, `MigrationError`, `AppliedMigration`, registry |
| `epoch_pg/src/migrations/m001_create_events_table.rs` | New: First migration (legacy table name) |
| `epoch_pg/src/migrations/m002_add_global_sequence.rs` | New: Second migration (legacy names) |
| `epoch_pg/src/migrations/m003_create_event_bus_infrastructure.rs` | New: Third migration (legacy names) |
| `epoch_pg/src/migrations/m004_rename_tables_with_epoch_prefix.rs` | New: Fourth migration (renames to epoch_* prefix) |
| `epoch_pg/src/event_store.rs` | Deprecate `initialize()` |
| `epoch_pg/src/event_bus.rs` | Deprecate `initialize()`, add `setup_trigger()` and `start_listener()` |
| `epoch_pg/Cargo.toml` | Add `sha2` and `async-trait` |

## 4. New Dependencies

```toml
sha2 = "0.10"           # For migration checksums
async-trait = "0.1"     # For async trait methods
```

## 5. Testing Strategy

1. **Unit tests** for `Migration` trait and checksum calculation
2. **Integration tests**:
   - Fresh database migration
   - Idempotent re-runs  
   - Pending/applied migration queries
   - Checksum mismatch detection
   - Backward compatibility with existing `initialize()` users
   - Complex migration with Rust logic (not just SQL)

## 6. Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Down migrations? | No | Schema rollbacks are risky in production. Keep it simple. |
| Transaction scope? | Per-migration | Each migration in its own transaction for atomicity. |
| CLI tool? | No | Users can integrate `Migrator` into their own tooling. |
| Migration definition | Trait-based | Allows complex Rust logic, not just raw SQL strings. |
| File organization | Separate files | Easier code review, cleaner git history. |
| Checksums | Yes | Detect tampering with shipped migrations. |
| Transaction control | Migrator-managed | Simpler API, consistent behavior. |

---

**Status**: ✅ Implemented
