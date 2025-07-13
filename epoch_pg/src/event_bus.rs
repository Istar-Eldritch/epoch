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
    D: EventData + Send + Sync + DeserializeOwned + 'static,
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
                        'purger_id', NEW.purger_id,
                        'data', NEW.data,
                        'created_at', NEW.created_at,
                        'purged_at', NEW.purged_at,
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

        let listener_pool = self.pool.clone();
        let mut listener = PgListener::connect_with(&listener_pool)
            .await
            .expect("TO be able to subscribe to postgres event stream");

        listener
            .listen(&self.channel_name)
            .await
            .expect("To be able to start listening to the event stream");

        let projections = self.projections.clone();

        tokio::spawn(async move {
            loop {
                while let Some(e) = listener.try_recv().await? {
                    let mut projections = projections.lock().await;
                    for projection in projections.iter_mut() {
                        let mut projection = projection.lock().await;
                        let db_event: PgDBEvent = serde_json::from_str(e.payload())?;
                        let data = db_event
                            .data
                            .map(|d| serde_json::from_value(d))
                            .transpose()?;
                        let event = Event {
                            id: db_event.id,
                            actor_id: db_event.actor_id,
                            stream_id: db_event.stream_id,
                            purger_id: db_event.purger_id,
                            event_type: db_event.event_type,
                            stream_version: db_event.stream_version as u64,
                            created_at: db_event.created_at,
                            purged_at: db_event.purged_at,
                            data,
                        };
                        projection.apply(&event);
                    }
                }
                // TODO: If we get here we have lost the connection and ideally we need to tell the
                // projecitons they will need to pull from the event stream directly before
                // applying the next event
            }
        });
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
