//! This module defines the `PgEventBus` that implements epoch_core::EventBus using PostgreSQL's
//! LISTEN/NOTIFY feature.
use crate::event_store::PgDBEvent;
use epoch_core::event::{Event, EventData};
use epoch_core::event_store::EventBus;
use epoch_core::prelude::EventObserver;
use log::{error, info};
use serde::de::DeserializeOwned;
use sqlx::Error as SqlxError;
use sqlx::postgres::{PgListener, PgPool};
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Duration, sleep};

use std::future::Future;

/// Errors that can occur when using `PgEventBus`.
#[derive(Debug, thiserror::Error)]
pub enum PgEventBusError {
    /// An error occurred with the SQLx library.
    #[error("SQLx error: {0}")]
    Sqlx(#[from] SqlxError),
    /// An error occurred during JSON serialization/deserialization.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// PostgreSQL implementation of `EventBus`.
/// Type alias for the projections collection to reduce type complexity.
type Projections<D> = Arc<Mutex<Vec<Arc<Mutex<dyn EventObserver<D>>>>>>;

/// PostgreSQL implementation of `EventBus`.
#[derive(Clone)]
pub struct PgEventBus<D>
where
    D: EventData + Send + Sync + DeserializeOwned,
{
    pool: PgPool,
    channel_name: String,
    projections: Projections<D>,
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
        // Create the function that will send the NOTIFY.
        // The function is renamed to be more specific and avoid potential conflicts.
        // It uses TG_ARGV[0] to get the channel name, which is safer than formatting it in.
        sqlx::query(
            r#"
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
                        'purged_at', NEW.purged_at
                    )::text
                );
                RETURN NEW;
            END;
            $$ LANGUAGE plpgsql;
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            DROP TRIGGER IF EXISTS event_bus_notify_trigger ON events;
        "#,
        )
        .execute(&self.pool)
        .await?;

        // Create the trigger that calls the function after an INSERT.
        // We escape single quotes in the channel name to prevent SQL injection.
        let create_trigger_query = format!(
            r#"
            CREATE TRIGGER event_bus_notify_trigger
            AFTER INSERT ON events
            FOR EACH ROW
            EXECUTE FUNCTION epoch_pg_notify_event('{}');
            "#,
            self.channel_name.replace('\'', "''")
        );

        sqlx::query(&create_trigger_query)
            .execute(&self.pool)
            .await?;

        let listener_pool = self.pool.clone();
        let channel_name = self.channel_name.clone();
        let projections = self.projections.clone();

        tokio::spawn(async move {
            let mut listener_option: Option<PgListener> = None;
            let mut reconnect_delay = Duration::from_secs(1);
            const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(60);

            loop {
                // Ensure listener is connected
                let listener = match listener_option {
                    Some(ref mut l) => l,
                    None => {
                        info!(
                            "Attempting to connect to PostgreSQL listener on channel '{}'",
                            channel_name
                        );
                        match PgListener::connect_with(&listener_pool).await {
                            Ok(mut l) => {
                                match l.listen(&channel_name).await {
                                    Ok(_) => {
                                        info!(
                                            "Successfully connected and listening on channel '{}'",
                                            channel_name
                                        );
                                        reconnect_delay = Duration::from_secs(1); // Reset delay on successful connection
                                        listener_option = Some(l);
                                        listener_option.as_mut().unwrap()
                                    }
                                    Err(e) => {
                                        error!(
                                            "Failed to listen on channel '{}': {}",
                                            channel_name, e
                                        );
                                        sleep(reconnect_delay).await;
                                        reconnect_delay =
                                            (reconnect_delay * 2).min(MAX_RECONNECT_DELAY);
                                        continue;
                                    }
                                }
                            }
                            Err(e) => {
                                error!("Failed to connect to PostgreSQL for listener: {}", e);
                                sleep(reconnect_delay).await;
                                reconnect_delay = (reconnect_delay * 2).min(MAX_RECONNECT_DELAY);
                                continue;
                            }
                        }
                    }
                };

                match listener.recv().await {
                    Ok(notification) => {
                        // Reset reconnect delay on successful message reception
                        reconnect_delay = Duration::from_secs(1);

                        let payload = notification.payload();
                        log::debug!("Received notification with payload: {}", payload);
                        let db_event: PgDBEvent = match serde_json::from_str(payload) {
                            Ok(event) => event,
                            Err(e) => {
                                error!(
                                    "Failed to deserialize PgDBEvent from payload '{}': {}",
                                    payload, e
                                );
                                continue; // Skip to next notification
                            }
                        };

                        let data =
                            match db_event.data.map(|d| serde_json::from_value(d)).transpose() {
                                Ok(data) => data,
                                Err(e) => {
                                    error!(
                                        "Failed to deserialize event data from payload '{}': {}",
                                        payload, e
                                    );
                                    continue; // Skip to next notification
                                }
                            };

                        let event = Event::<D> {
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

                        // Wrap event in Arc once for efficient sharing across projections
                        let event = Arc::new(event);

                        let mut projections_guard = projections.lock().await;
                        for projection in projections_guard.iter_mut() {
                            let projection_guard = projection.lock().await;
                            log::debug!("Applying event to projection: {:?}", event.id);
                            match projection_guard.on_event(Arc::clone(&event)).await {
                                Ok(_) => {
                                    log::debug!(
                                        "Successfully applied event to projection: {:?}",
                                        event.id
                                    );
                                }
                                Err(e) => {
                                    error!("Failed applying event to projection: {:?}", e);
                                    // TODO: Send event to DLQ. & Retry
                                    continue;
                                }
                            };
                        }
                    }
                    Err(e) => {
                        error!(
                            "Error receiving notification: {}. Attempting to reconnect...",
                            e
                        );
                        // Invalidate the listener to force a reconnection attempt
                        listener_option = None;
                        sleep(reconnect_delay).await;
                        reconnect_delay = (reconnect_delay * 2).min(MAX_RECONNECT_DELAY);
                    }
                }
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

    /// This is NO-OP. Use the PGEventStore to push events to the events table. Subscribers to this
    /// bus will receive the notifications
    fn publish<'a>(
        &'a self,
        _event: Arc<Event<Self::EventType>>,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        // This is a noop. Use the PgEventStore to add events to the event table.
        Box::pin(async { Ok(()) })
    }

    fn subscribe<T>(
        &self,
        projector: T,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), Self::Error>> + Send>>
    where
        T: EventObserver<Self::EventType> + 'static,
    {
        let projections = self.projections.clone();
        Box::pin(async move {
            let mut projections = projections.lock().await;
            projections.push(Arc::new(Mutex::new(projector)));
            Ok(())
        })
    }
}
