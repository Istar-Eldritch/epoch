use async_stream::try_stream;
use epoch_core::event::{Event, EventData};
use epoch_core::prelude::{EventBus, EventStoreBackend, EventStream};
use futures_util::{Stream, StreamExt};
use serde::Serialize;
use serde::{Deserialize, de::DeserializeOwned};
use sqlx::{FromRow, PgPool};
use std::{future::Future, pin::Pin, task::Poll};
use uuid::Uuid;

/// A postgres based event store.
///
#[derive(Clone, Debug)]
pub struct PgEventStore<B: EventBus + Clone> {
    postgres: PgPool,
    bus: B,
}

impl<B: EventBus + Clone> PgEventStore<B> {
    /// Creates a new `PgEventStore`.
    pub fn new(postgres: PgPool, bus: B) -> Self {
        log::debug!("Creating a new PgEventStore");
        Self { postgres, bus }
    }

    /// Exposes the event store bus
    pub fn bus(&self) -> &B {
        &self.bus
    }

    /// Initializes the event store, creating the events table if it does not exist.
    pub async fn initialize(&self) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS events (
                id UUID PRIMARY KEY,
                stream_id UUID NOT NULL,
                stream_version INT NOT NULL,
                event_type VARCHAR(255) NOT NULL,
                data JSONB,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                UNIQUE (stream_id, stream_version)
            );
            "#,
        )
        .execute(&self.postgres)
        .await?;

        Ok(())
    }
}

/// Postgres representation of the event
#[derive(Debug, FromRow, Serialize, Deserialize)]
pub struct PgDBEvent {
    /// The id of the event
    pub id: Uuid,
    /// The steam this event belongs to
    pub stream_id: Uuid,
    /// The stream version, used for conflict checks
    pub stream_version: i32,
    /// Who created the event
    pub actor_id: Option<Uuid>,
    /// The type of the event
    pub event_type: String,
    /// The data of the event
    pub data: Option<serde_json::Value>,
    /// When the event was created
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// If this event was purged, who purged it.
    pub purger_id: Option<Uuid>,
    /// If this event was purged, when it was purged
    pub purged_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// A postgres based event stream.
pub struct PgEventStream<'a, D>
where
    D: EventData + Send + Sync + 'a,
{
    inner: Pin<
        Box<
            dyn Stream<Item = Result<Event<D>, Box<dyn std::error::Error + Send + Sync>>>
                + Send
                + 'a,
        >,
    >,
}

impl<'a, D> Stream for PgEventStream<'a, D>
where
    D: EventData + Send + Sync + 'a,
{
    type Item = Result<Event<D>, Box<dyn std::error::Error + Send + Sync>>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

impl<'a, D> EventStream<D> for PgEventStream<'a, D> where D: EventData + Send + Sync + 'a {}

/// Errors returned by the PgEventStore
#[derive(Debug, thiserror::Error)]
pub enum PgEventStoreError<BE>
where
    BE: std::error::Error,
{
    /// A database error
    #[error("Database error: {0}")]
    DBError(#[from] sqlx::error::Error),
    /// A bus error
    #[error("Publish error: {0}")]
    BUSPublishError(BE),
    /// Error deserializing the event from the database
    #[error("Deserialize event error: {0}")]
    DeserializeEventError(#[from] serde_json::Error),
    /// Errors building the event from the db representation
    #[error("Build event error: {0}")]
    BuildEventError(#[from] epoch_core::event::EventBuilderError),
}

impl<'a, B> EventStoreBackend<'a> for PgEventStore<B>
where
    B: EventBus + Send + Sync + Clone + 'a,
    B::EventType: Send + Sync + DeserializeOwned + 'static,
    B::Error: Send + Sync + 'static,
{
    type EventType = B::EventType;
    type Error = PgEventStoreError<B::Error>;

    fn read_events(
        &'a self,
        stream_id: Uuid,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        Pin<Box<dyn EventStream<Self::EventType> + Send + 'a>>,
                        Self::Error,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let stream = try_stream! {
                let mut inner_stream = sqlx::query_as::<_, PgDBEvent>(
                    r#"
                    SELECT
                        id,
                        stream_id,
                        stream_version,
                        event_type,
                        data,
                        created_at
                    FROM events
                    WHERE stream_id = $1
                    ORDER BY stream_version ASC
                    "#,
                )
                .bind(stream_id)
                .fetch(&self.postgres);

                while let Some(row) = inner_stream.next().await {
                    let entry: PgDBEvent = row.map_err(PgEventStoreError::DBError::<B::Error>)?;

                    let data: Option<B::EventType> = entry
                        .data
                        .map(|d| serde_json::from_value(d))
                        .transpose()
                        .map_err(PgEventStoreError::DeserializeEventError::<B::Error>)?;

                    let event = Event::<B::EventType>::builder()
                        .id(entry.id)
                        .stream_id(entry.stream_id)
                        .stream_version(entry.stream_version.try_into().unwrap())
                        .event_type(entry.event_type)
                        .created_at(entry.created_at)
                        .data(data)
                        .build()
                        .map_err(PgEventStoreError::BuildEventError::<B::Error>)?;
                    yield event;
                }
            };

            // let stream =
            //     stream.map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>);

            let event_stream: Pin<Box<dyn EventStream<Self::EventType> + Send + 'a>> =
                Box::pin(PgEventStream {
                    inner: Box::pin(stream),
                });

            Ok(event_stream)
        })
    }

    fn store_event(
        &'a self,
        event: Event<Self::EventType>,
    ) -> Pin<Box<dyn Future<Output = Result<Event<Self::EventType>, Self::Error>> + Send + 'a>>
    {
        Box::pin(async move {
            sqlx::query(
                r#"
                INSERT INTO events (id, stream_id, stream_version, event_type, data, created_at)
                VALUES ($1, $2, $3, $4, $5, $6)
                "#,
            )
            .bind(event.id)
            .bind(event.stream_id)
            .bind(event.stream_version as i64)
            .bind(event.event_type.to_string())
            .bind(
                event
                    .data
                    .as_ref()
                    .map(|d| serde_json::to_value(d))
                    .transpose()?,
            )
            .bind(event.created_at)
            .execute(&self.postgres)
            .await?;

            self.bus
                .publish(event.clone())
                .await
                .map_err(|e| PgEventStoreError::BUSPublishError(e))?;

            Ok(event)
        })
    }
}
