//! This module defines the `PgEventBus` that implements epoch_core::EventBus using PostgreSQL's
//! LISTEN/NOTIFY feature.
use crate::event_store::PgDBEvent;
use epoch_core::event::{Event, EventData};
use epoch_core::event_store::EventBus;
use epoch_core::prelude::Projection;
use futures::StreamExt;
use serde::de::DeserializeOwned;
use sqlx::Error as SqlxError;
use sqlx::postgres::{PgListener, PgPool};
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Errors that can occur when using `PgEventBus`.
#[derive(Debug, thiserror::Error)]
pub enum PgEventBusError {
    /// An error occurred with the SQLx library.
    #[error("SQLx error: {0}")]
    Sqlx(#[from] SqlxError),
    /// An error occurred during JSON serialization/deserialization.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// An error occurred when the event data is missing during deserialization.
    #[error("Event data missing in notification payload")]
    EventDataMissing,
}

/// PostgreSQL implementation of `EventBus`.
pub struct PgEventBus<D>
where
    D: EventData + Send + Sync + DeserializeOwned,
{
    pool: PgPool,
    channel_name: String,
    projections: Arc<Mutex<Vec<Arc<Mutex<dyn Projection<D>>>>>>,
}

impl<D> PgEventBus<D>
where
    D: EventData + Send + Sync + DeserializeOwned,
{
    /// Creates a new `PgEventBus` instance.
    pub fn new(pool: PgPool, channel_name: impl Into<String>) -> Self {
        Self {
            pool,
            channel_name: channel_name.into(),
            projections: Arc::new(Mutex::new(vec![])),
        }
    }

    /// Creates the necessary trigger function and trigger for notifications.
    pub async fn initialize(&self) -> Result<(), SqlxError> {
        // Create the function that will send the NOTIFY
        sqlx::query(&format!(
            r#"
            CREATE OR REPLACE FUNCTION notify_event_bus()
            RETURNS TRIGGER AS $$
            BEGIN
                PERFORM pg_notify(
                    '{}',
                    json_build_object(
                        'id', NEW.id,
                        'stream_id', NEW.stream_id,
                        'stream_version', NEW.stream_version,
                        'event_type', NEW.event_type,
                        'actor_id', NEW.actor_id,
                        'data', NEW.data,
                        'created_at', NEW.created_at,
                    )::text
                );
                RETURN NEW;
            END;
            $$ LANGUAGE plpgsql;
            "#,
            self.channel_name
        ))
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            DROP TRIGGER IF EXISTS event_bus_notify_trigger ON events;
        "#,
        )
        .execute(&self.pool)
        .await?;

        // Create the trigger that calls the function after an INSERT
        sqlx::query(
            r#"
            CREATE TRIGGER event_bus_notify_trigger
            AFTER INSERT ON events
            FOR EACH ROW
            EXECUTE FUNCTION notify_event_bus();
            "#,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

impl<D> EventBus for PgEventBus<D>
where
    D: EventData + Send + Sync + DeserializeOwned + 'static,
{
    type EventType = D;
    type Error = PgEventBusError;

    /// This is NOOP. Use the PGEventStore to push events to the events table. Subscribers to this
    /// bus will receive the notifications
    fn publish<'a>(
        &'a self,
        _event: Event<Self::EventType>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        // This is a noop. Use the PgEventStore to add events to the event table.
        Box::pin(async { Ok(()) })
    }

    fn subscribe(
        &self,
        projector: Arc<Mutex<dyn Projection<Self::EventType>>>,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), Self::Error>> + Send>> {
        let projections = self.projections.clone();
        Box::pin(async move {
            let mut projections = projections.lock().await;
            projections.push(projector);
            Ok(())
        })
    }
}
