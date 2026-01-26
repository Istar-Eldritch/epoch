//! # Epoch memory store
//!
//! In-memory implementations of epoch's storage backends, primarily for testing.
//!
//! # Transaction Support
//!
//! This crate provides [`InMemoryTransaction`] for testing transactional aggregate
//! workflows without a database:
//!
//! ```ignore
//! use std::sync::Arc;
//! use epoch_core::aggregate::TransactionalAggregate;
//!
//! // Assuming you have an aggregate that implements TransactionalAggregate
//! let aggregate = Arc::new(my_in_memory_aggregate);
//!
//! // Begin a transaction
//! let mut tx = aggregate.clone().begin().await?;
//!
//! // Handle multiple commands atomically
//! tx.handle(create_command).await?;
//! tx.handle(update_command).await?;
//!
//! // Commit persists all changes and publishes events
//! tx.commit().await?;
//!
//! // Or rollback to discard all changes
//! // tx.rollback().await?;
//! ```
//!
//! See [`InMemoryTransaction`] for implementation details and
//! [`InMemoryTransactionError`] for error handling.

#![deny(missing_docs)]

mod aggregate;
mod event_store;

pub use aggregate::*;
pub use event_store::*;
