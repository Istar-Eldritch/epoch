# Specification: PostgreSQL Migration Mechanism

**Spec ID:** 0003  
**Status:** ✅ Implemented  
**Created:** 2026-01-26  
**Completed:** 2026-01-26  

## Problem Statement

Schema setup was handled through ad-hoc `initialize()` methods that:
- Mixed table creation with evolution logic
- Used hard-to-maintain `DO $$ ... $$` blocks
- Didn't provide clear history of changes
- Required users to call multiple methods in correct order
- Made schema changes difficult to track and test

## Solution Overview

Implemented a simple embedded migration system:
- Versioned, sequential migrations
- Idempotent (safe to run multiple times)
- Trait-based (allows complex Rust logic, not just SQL)
- Tracking table for applied migrations
- Single entry point replaces multiple `initialize()` calls

### Core Components

**Migration Tracking Table:**
```sql
CREATE TABLE _epoch_migrations (
    version BIGINT PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    applied_at TIMESTAMPTZ NOT NULL,
    checksum VARCHAR(64) NOT NULL
);
```

**Migration Trait:**
```rust
trait Migration: Send + Sync {
    fn version(&self) -> i64;
    fn name(&self) -> &'static str;
    async fn up(&self, tx: &mut Transaction<Postgres>) -> Result<...>;
    fn checksum(&self) -> String;  // Default: SHA-256 of version + name
}
```

**Migrator:**
```rust
struct Migrator {
    pool: PgPool,
}

impl Migrator {
    fn new(pool: PgPool) -> Self;
    async fn run(&self) -> Result<usize, MigrationError>;  // Returns # applied
    async fn current_version(&self) -> Result<i64, MigrationError>;
    async fn pending(&self) -> Result<Vec<&'static dyn Migration>, MigrationError>;
    async fn applied(&self) -> Result<Vec<AppliedMigration>, MigrationError>;
}
```

## Key Design Decisions

### Why Embedded Migrations?

- No external tools needed
- Migrations compiled into library
- Type-safe Rust logic when needed
- Simple for end users

### Why Trait-Based?

Allows complex logic beyond raw SQL:
```rust
async fn up(&self, tx: &mut Transaction<Postgres>) -> Result<...> {
    // Can do conditional logic
    if some_condition {
        sqlx::query("...").execute(tx).await?;
    }
    // Can call helper functions
    migrate_data(tx).await?;
    Ok(())
}
```

### Why One Transaction Per Migration?

- Atomic: migration succeeds or fails as a unit
- Partial progress: successfully applied migrations remain committed
- Simpler error recovery than nested transactions

### Table Naming Convention

All epoch tables use `epoch_` prefix to avoid conflicts:
- `_epoch_migrations` (underscore indicates internal/meta table)
- `epoch_events`
- `epoch_event_bus_checkpoints`
- `epoch_event_bus_dlq`

### Migration Strategy for Existing Databases

Migrations 1-3 create tables with **legacy names** (for backward compatibility):
- Uses `IF NOT EXISTS` so existing tables preserved
- Migration 4 renames all to `epoch_` prefix
- Handles both fresh installs and upgrades gracefully

## Migration Files

Each migration in its own file:
```
epoch_pg/src/migrations/
  mod.rs                              # Migrator, trait, registry
  m001_create_events_table.rs
  m002_add_global_sequence.rs
  m003_create_event_bus_infrastructure.rs
  m004_rename_tables_with_epoch_prefix.rs
```

Registry in `mod.rs`:
```rust
const MIGRATIONS: &[&dyn Migration] = &[
    &CreateEventsTable,
    &AddGlobalSequence,
    &CreateEventBusInfrastructure,
    &RenameTablesWithEpochPrefix,
];
```

## Usage

```rust
let pool = PgPool::connect("postgres://...").await?;

// Run migrations (replaces initialize() calls)
let migrator = Migrator::new(pool.clone());
let applied = migrator.run().await?;
println!("Applied {} migrations", applied);

// Create event bus and store
let event_bus = PgEventBus::new(pool.clone(), "my_channel");
let event_store = PgEventStore::new(pool.clone(), event_bus.clone());

// Set up channel-specific trigger
event_bus.setup_trigger().await?;
event_bus.start_listener().await?;
```

## Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Down migrations? | No | Risky in production, keep simple |
| Transaction scope | Per-migration | Atomic units with partial progress |
| CLI tool? | No | Users integrate into their tooling |
| Definition | Trait-based | Enables complex Rust logic |
| Organization | Separate files | Easier review, cleaner git history |
| Checksums | Yes | Detect tampering with shipped migrations |

## Backward Compatibility

`initialize()` methods marked `#[deprecated]` and internally call migrator.

Existing databases work - migrations use `IF NOT EXISTS` checks.

## Trigger Setup

Channel-specific trigger remains separate from migrations:
- Migrations handle schema, not channel-specific config
- `setup_trigger()` method on `PgEventBus` creates trigger for specific channel
- Cleaner separation of concerns
