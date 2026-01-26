use async_stream::try_stream;
use async_trait::async_trait;
use epoch_core::event::{Event, EventData};
use epoch_core::prelude::{EventBus, EventStoreBackend, EventStream};
use futures_util::{Stream, StreamExt};
use serde::Serialize;
use serde::{Deserialize, de::DeserializeOwned};
use sqlx::{FromRow, PgPool};
use std::{pin::Pin, task::Poll};
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

    /// Returns the PostgreSQL connection pool.
    ///
    /// This is useful for operations that need direct database access,
    /// such as catch-up queries in the event bus.
    pub fn pool(&self) -> &PgPool {
        &self.postgres
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
    pub stream_version: i64,
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
    /// Global sequence number for ordering across all streams.
    /// Assigned by the database on insert using a sequence.
    #[sqlx(default)]
    pub global_sequence: Option<i64>,
}

/// A postgres based event stream.
pub struct PgEventStream<'a, D, E>
where
    D: EventData + Send + Sync + 'a,
    E: std::error::Error + Send + Sync,
{
    inner: Pin<Box<dyn Stream<Item = Result<Event<D>, E>> + Send + 'a>>,
}

impl<'a, D, E> Stream for PgEventStream<'a, D, E>
where
    D: EventData + Send + Sync + 'a,
    E: std::error::Error + Send + Sync,
{
    type Item = Result<Event<D>, E>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

impl<'a, D, E> EventStream<D, E> for PgEventStream<'a, D, E>
where
    D: EventData + Send + Sync + 'a,
    E: std::error::Error + Send + Sync,
{
}

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

#[async_trait]
impl<B> EventStoreBackend for PgEventStore<B>
where
    B: EventBus + Send + Sync + Clone + 'static,
    B::EventType: Send + Sync + DeserializeOwned + 'static,
    B::Error: Send + Sync + 'static,
{
    type EventType = B::EventType;
    type Error = PgEventStoreError<B::Error>;

    async fn read_events(
        &self,
        stream_id: Uuid,
    ) -> Result<Pin<Box<dyn EventStream<Self::EventType, Self::Error> + Send + 'life0>>, Self::Error>
    {
        self.read_events_since(stream_id, 0).await
    }

    async fn read_events_since(
        &self,
        stream_id: Uuid,
        version: u64,
    ) -> Result<Pin<Box<dyn EventStream<Self::EventType, Self::Error> + Send + 'life0>>, Self::Error>
    {
        let stream = try_stream! {
            let mut inner_stream = sqlx::query_as::<_, PgDBEvent>(
                r#"
                    SELECT
                        id,
                        stream_id,
                        stream_version,
                        event_type,
                        data,
                        created_at,
                        actor_id,
                        purger_id,
                        purged_at,
                        global_sequence
                    FROM epoch_events
                    WHERE stream_id = $1 AND stream_version >= $2
                    ORDER BY stream_version ASC
                    "#,
            )
            .bind(stream_id)
            .bind(version as i64)
            .fetch(&self.postgres);

            while let Some(row) = inner_stream.next().await {
                let entry: PgDBEvent = row.map_err(PgEventStoreError::DBError::<B::Error>)?;

                let data: Option<B::EventType> = entry
                    .data
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(PgEventStoreError::DeserializeEventError::<B::Error>)?;

                let mut builder = Event::<B::EventType>::builder()
                    .id(entry.id)
                    .stream_id(entry.stream_id)
                    .stream_version(entry.stream_version.try_into().unwrap())
                    .event_type(entry.event_type)
                    .created_at(entry.created_at)
                    .data(data);

                // Add global_sequence if present
                if let Some(gs) = entry.global_sequence {
                    builder = builder.global_sequence(gs as u64);
                }

                let event = builder
                    .build()
                    .map_err(PgEventStoreError::BuildEventError::<B::Error>)?;
                yield event;
            }
        };

        let event_stream: Pin<Box<dyn EventStream<Self::EventType, Self::Error> + Send + 'life0>> =
            Box::pin(PgEventStream {
                inner: Box::pin(stream),
            });

        Ok(event_stream)
    }

    async fn store_event(&self, event: Event<Self::EventType>) -> Result<(), Self::Error> {
        // Insert the event and get back the assigned global_sequence
        let row: (i64,) = sqlx::query_as(
                r#"
                INSERT INTO epoch_events (id, stream_id, stream_version, event_type, data, created_at, actor_id, purger_id, purged_at)
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                RETURNING global_sequence
                "#,
            )
            .bind(event.id)
            .bind(event.stream_id)
            .bind(TryInto::<i64>::try_into(event.stream_version).map_err(|e| {
                PgEventStoreError::DBError(sqlx::error::Error::InvalidArgument(format!(
                    "stream_version {} is too large to fit in i64: {}",
                    event.stream_version, e
                )))
            })?)
            .bind(event.event_type.to_string())
            .bind(
                event
                    .data
                    .as_ref()
                    .map(serde_json::to_value)
                    .transpose()?,
            )
            .bind(event.created_at)
            .bind(event.actor_id)
            .bind(event.purger_id)
            .bind(event.purged_at)
            .fetch_one(&self.postgres)
            .await?;

        // Create a new event with the assigned global_sequence
        let event_with_sequence = Event {
            id: event.id,
            stream_id: event.stream_id,
            stream_version: event.stream_version,
            event_type: event.event_type,
            actor_id: event.actor_id,
            purger_id: event.purger_id,
            data: event.data,
            created_at: event.created_at,
            purged_at: event.purged_at,
            global_sequence: Some(row.0 as u64),
        };

        // Wrap in Arc for efficient sharing - no clone needed
        self.bus
            .publish(std::sync::Arc::new(event_with_sequence))
            .await
            .map_err(PgEventStoreError::BUSPublishError)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pg_db_event_serialization_with_global_sequence() {
        let db_event = PgDBEvent {
            id: Uuid::new_v4(),
            stream_id: Uuid::new_v4(),
            stream_version: 1,
            actor_id: None,
            event_type: "TestEvent".to_string(),
            data: Some(serde_json::json!({"key": "value"})),
            created_at: chrono::Utc::now(),
            purger_id: None,
            purged_at: None,
            global_sequence: Some(123),
        };

        let json = serde_json::to_string(&db_event).unwrap();
        let parsed: PgDBEvent = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.global_sequence, Some(123));
        assert_eq!(parsed.id, db_event.id);
        assert_eq!(parsed.stream_id, db_event.stream_id);
        assert_eq!(parsed.stream_version, db_event.stream_version);
        assert_eq!(parsed.event_type, db_event.event_type);
    }

    #[test]
    fn pg_db_event_serialization_without_global_sequence() {
        let db_event = PgDBEvent {
            id: Uuid::new_v4(),
            stream_id: Uuid::new_v4(),
            stream_version: 1,
            actor_id: None,
            event_type: "TestEvent".to_string(),
            data: None,
            created_at: chrono::Utc::now(),
            purger_id: None,
            purged_at: None,
            global_sequence: None,
        };

        let json = serde_json::to_string(&db_event).unwrap();
        let parsed: PgDBEvent = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.global_sequence, None);
    }

    #[test]
    fn pg_db_event_deserialization_missing_global_sequence_field() {
        // Simulate a JSON payload from an older version without global_sequence
        let json = r#"{
            "id": "550e8400-e29b-41d4-a716-446655440000",
            "stream_id": "550e8400-e29b-41d4-a716-446655440001",
            "stream_version": 1,
            "actor_id": null,
            "event_type": "TestEvent",
            "data": null,
            "created_at": "2026-01-21T00:00:00Z",
            "purger_id": null,
            "purged_at": null
        }"#;

        let parsed: PgDBEvent = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.global_sequence, None);
    }
}
