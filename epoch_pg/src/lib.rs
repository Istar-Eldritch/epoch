//! # Epoch postgres store
//!
//! PostgreSQL implementations of epoch's storage backends for production use.
//!
//! # Transaction Support
//!
//! This crate provides [`PgTransaction`] for atomic aggregate operations with
//! row-level locking via `SELECT ... FOR UPDATE`:
//!
//! ```ignore
//! use std::sync::Arc;
//! use epoch_core::aggregate::TransactionalAggregate;
//!
//! // Assuming you have an aggregate that implements TransactionalAggregate
//! // using the impl_pg_transactional_aggregate! macro
//! let aggregate = Arc::new(my_pg_aggregate);
//!
//! // Begin a transaction (acquires database transaction)
//! let mut tx = aggregate.clone().begin().await?;
//!
//! // Handle commands - state is locked with FOR UPDATE
//! tx.handle(create_command).await?;
//! tx.handle(update_command).await?;
//!
//! // Commit persists all changes atomically, then publishes events
//! tx.commit().await?;
//!
//! // Or rollback to discard all changes (no events published)
//! // tx.rollback().await?;
//! ```
//!
//! # Key Features
//!
//! - **Pessimistic locking**: `FOR UPDATE` prevents concurrent modifications
//! - **Atomic commits**: All state changes commit in a single transaction
//! - **Ordered publishing**: Events published only after successful commit
//! - **Deadlock detection**: PostgreSQL detects and reports deadlocks
//!
//! See [`PgTransaction`], [`aggregate`] module, and
//! [`PgAggregateError`] for implementation details.

#![deny(missing_docs)]

/// Aggregate with transaction support for PostgreSQL
pub mod aggregate;

/// The event_bus module exports implementations of the EventBus for postgres
pub mod event_bus;

/// The event_store module exports implementations of the EventStoreBackend for postgres
pub mod event_store;

/// Database migrations for epoch_pg schema management
pub mod migrations;

/// The state store implementation for postgres
pub mod state_store;

pub use aggregate::*;
pub use event_bus::{
    CheckpointMode, DlqCallback, DlqEntry, DlqInsertionInfo, InstanceMode, PgEventBus,
    PgEventBusError, ReliableDeliveryConfig,
};
pub use event_store::*;
pub use migrations::{AppliedMigration, Migration, MigrationError, Migrator};
pub use state_store::*;
