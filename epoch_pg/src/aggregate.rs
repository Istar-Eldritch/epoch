//! PostgreSQL aggregate with transaction support.
//!
//! This module provides [`PgAggregate`], a PostgreSQL-backed aggregate that implements
//! [`TransactionalAggregate`] for atomic command handling.
//!
//! # Example
//!
//! ```ignore
//! use epoch_pg::PgAggregate;
//! use std::sync::Arc;
//!
//! // Create aggregate with pool, event store, and state store
//! let aggregate = Arc::new(PgAggregate::new(
//!     pool.clone(),
//!     event_store,
//!     state_store,
//!     |state, cmd| { /* handle_command logic */ },
//!     |state, event| { /* apply logic */ },
//! ));
//!
//! // Use transaction for atomic operations
//! let mut tx = aggregate.clone().begin().await?;
//! tx.handle(command).await?;
//! tx.commit().await?;
//! ```

use crate::event_store::PgEventStoreError;
use async_trait::async_trait;
use epoch_core::aggregate::TransactionOps;
use sqlx::Postgres;

/// Errors for PostgreSQL aggregate operations.
#[derive(Debug, thiserror::Error)]
pub enum PgAggregateError<BE>
where
    BE: std::error::Error,
{
    /// Database error.
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    /// Event store error.
    #[error("Event store error: {0}")]
    EventStore(PgEventStoreError<BE>),
}

impl<BE> From<PgEventStoreError<BE>> for PgAggregateError<BE>
where
    BE: std::error::Error,
{
    fn from(e: PgEventStoreError<BE>) -> Self {
        PgAggregateError::EventStore(e)
    }
}

/// Wrapper for sqlx transaction that implements [`TransactionOps`].
///
/// The generic parameter `E` is the error type that `sqlx::Error` will be converted into.
/// Typically this is `PgAggregateError<BE>` where `BE` is the event bus error type.
///
/// Access to the underlying transaction is provided via `Deref` and `DerefMut`,
/// allowing use as a sqlx executor (e.g., `&mut *tx`).
pub struct PgTransaction<E>(
    sqlx::Transaction<'static, Postgres>,
    std::marker::PhantomData<E>,
);

impl<E> PgTransaction<E> {
    /// Creates a new PgTransaction wrapper.
    pub fn new(tx: sqlx::Transaction<'static, Postgres>) -> Self {
        Self(tx, std::marker::PhantomData)
    }
}

#[async_trait]
impl<E> TransactionOps for PgTransaction<E>
where
    E: std::error::Error + Send + Sync + 'static + From<sqlx::Error>,
{
    type Error = E;

    async fn commit(self) -> Result<(), Self::Error> {
        self.0.commit().await.map_err(Into::into)
    }

    async fn rollback(self) -> Result<(), Self::Error> {
        self.0.rollback().await.map_err(Into::into)
    }
}

impl<E> std::ops::Deref for PgTransaction<E> {
    type Target = sqlx::Transaction<'static, Postgres>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<E> std::ops::DerefMut for PgTransaction<E> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
