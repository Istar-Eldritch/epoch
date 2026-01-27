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

// ============================================================================
// Helper Macro for TransactionalAggregate Implementation
// ============================================================================

/// Implements `TransactionalAggregate` for a PostgreSQL aggregate.
///
/// This macro reduces the boilerplate of implementing `TransactionalAggregate`
/// for aggregates that use PostgreSQL stores.
///
/// # Example
///
/// ```ignore
/// use epoch_pg::impl_pg_transactional_aggregate;
///
/// pub struct MyAggregate {
///     pool: PgPool,
///     event_store: PgEventStore<MyEventBus>,
///     state_store: PgStateStore<MyState>,
/// }
///
/// impl_pg_transactional_aggregate! {
///     aggregate: MyAggregate,
///     event: MyEvent,
///     state: MyState,
///     bus: MyEventBus,
///     pool_field: pool,
///     event_store_field: event_store,
/// }
/// ```
#[macro_export]
macro_rules! impl_pg_transactional_aggregate {
    (
        aggregate: $aggregate:ty,
        event: $event:ty,
        state: $state:ty,
        bus: $bus:ty,
        pool_field: $pool_field:ident,
        event_store_field: $event_store_field:ident$(,)?
    ) => {
        #[::async_trait::async_trait]
        impl ::epoch_core::aggregate::TransactionalAggregate for $aggregate {
            type SupersetEvent = $event;
            type Transaction = $crate::aggregate::PgTransaction<
                $crate::aggregate::PgAggregateError<<$bus as ::epoch_core::event_store::EventBus>::Error>,
            >;
            type TransactionError = $crate::aggregate::PgAggregateError<
                <$bus as ::epoch_core::event_store::EventBus>::Error,
            >;

            async fn begin(
                self: ::std::sync::Arc<Self>,
            ) -> ::std::result::Result<
                ::epoch_core::aggregate::AggregateTransaction<Self, Self::Transaction>,
                Self::TransactionError,
            > {
                let tx = self.$pool_field.begin().await?;
                Ok(::epoch_core::aggregate::AggregateTransaction::new(
                    self,
                    $crate::aggregate::PgTransaction::new(tx),
                ))
            }

            async fn store_events_in_tx(
                &self,
                tx: &mut Self::Transaction,
                events: ::std::vec::Vec<::epoch_core::event::Event<Self::SupersetEvent>>,
            ) -> ::std::result::Result<
                ::std::vec::Vec<::epoch_core::event::Event<Self::SupersetEvent>>,
                Self::TransactionError,
            > {
                self.$event_store_field
                    .store_events_in_tx(&mut **tx, events)
                    .await
                    .map_err($crate::aggregate::PgAggregateError::EventStore)
            }

            async fn get_state_in_tx(
                &self,
                tx: &mut Self::Transaction,
                id: ::uuid::Uuid,
            ) -> ::std::result::Result<::std::option::Option<$state>, Self::TransactionError> {
                use $crate::state_store::PgState;
                <$state>::find_by_id_for_update(id, tx.as_mut())
                    .await
                    .map_err(Into::into)
            }

            async fn persist_state_in_tx(
                &self,
                tx: &mut Self::Transaction,
                id: ::uuid::Uuid,
                state: $state,
            ) -> ::std::result::Result<(), Self::TransactionError> {
                use $crate::state_store::PgState;
                <$state>::upsert(id, &state, tx.as_mut())
                    .await
                    .map_err(Into::into)
            }

            async fn delete_state_in_tx(
                &self,
                tx: &mut Self::Transaction,
                id: ::uuid::Uuid,
            ) -> ::std::result::Result<(), Self::TransactionError> {
                use $crate::state_store::PgState;
                <$state>::delete(id, tx.as_mut())
                    .await
                    .map_err(Into::into)
            }

            async fn publish_event(
                &self,
                event: ::epoch_core::event::Event<Self::SupersetEvent>,
            ) -> ::std::result::Result<(), Self::TransactionError> {
                self.$event_store_field
                    .publish_events(::std::vec![event])
                    .await
                    .map_err($crate::aggregate::PgAggregateError::EventStore)
            }
        }
    };
}
