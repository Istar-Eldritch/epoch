use async_stream::try_stream;
use async_trait::async_trait;
use epoch_core::event::{Event, EventData};
use epoch_core::prelude::{EventBus, EventStoreBackend, EventStream};
use epoch_core::upcasting::UpcasterRegistry;
use futures_util::{Stream, StreamExt};
use serde::Serialize;
use serde::{Deserialize, de::DeserializeOwned};
use sqlx::{FromRow, PgPool};
use std::sync::Arc;
use std::{pin::Pin, task::Poll};
use uuid::Uuid;

/// A postgres based event store.
///
#[derive(Clone)]
pub struct PgEventStore<B: EventBus + Clone> {
    postgres: PgPool,
    bus: B,
    events_table: String,
    upcasters: Arc<UpcasterRegistry>,
}

impl<B: EventBus + Clone> std::fmt::Debug for PgEventStore<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgEventStore")
            .field("events_table", &self.events_table)
            .finish_non_exhaustive()
    }
}

impl<B: EventBus + Clone> PgEventStore<B> {
    /// Creates a new `PgEventStore` writing to the default `epoch_events` table.
    pub fn new(postgres: PgPool, bus: B) -> Self {
        log::debug!("Creating a new PgEventStore");
        Self {
            postgres,
            bus,
            events_table: "epoch_events".to_string(),
            upcasters: Arc::new(UpcasterRegistry::new()),
        }
    }

    /// Creates a new `PgEventStore` writing to the default `epoch_events` table
    /// with a configured [`UpcasterRegistry`].
    ///
    /// The registry is consulted on every read at the deserialization boundary
    /// ([`pg_db_event_to_event`]): stored payloads are upcast forward to the
    /// current schema version before being deserialized into the domain
    /// [`EventData`] type, applying the registry's configured
    /// [`FailurePolicy`](epoch_core::upcasting::FailurePolicy). The default
    /// constructors install an empty registry, which is a no-op for any event
    /// that still deserializes cleanly.
    pub fn with_upcasters(postgres: PgPool, bus: B, upcasters: Arc<UpcasterRegistry>) -> Self {
        log::debug!("Creating a new PgEventStore with an upcaster registry");
        Self {
            postgres,
            bus,
            events_table: "epoch_events".to_string(),
            upcasters,
        }
    }

    /// Creates a new `PgEventStore` writing to a custom events table.
    ///
    /// This is `async` because it ensures the CLOUD-180 `txid` column (used for
    /// snapshot-fencing forensics) exists on the custom table, applying an
    /// idempotent `ADD COLUMN IF NOT EXISTS` + `SET DEFAULT` + partial index.
    /// The default `epoch_events` table (via [`new`](Self::new)) is covered by
    /// migration m011 and does not need this. If ensuring the column fails the
    /// error is logged and construction still succeeds — fencing simply degrades
    /// to timeout-only for this table.
    pub async fn with_table(postgres: PgPool, bus: B, events_table: impl Into<String>) -> Self {
        let events_table = events_table.into();
        log::debug!("Creating a new PgEventStore targeting table '{events_table}'");
        if let Err(e) = crate::event_bus::ensure_txid_column(&postgres, &events_table).await {
            log::warn!(
                "Failed to ensure txid column on custom events table '{events_table}'; \
                 snapshot fencing degrades to timeout-only for this table: {e}"
            );
        }
        if let Err(e) =
            crate::event_bus::ensure_schema_version_column(&postgres, &events_table).await
        {
            log::warn!(
                "Failed to ensure schema_version column on custom events table '{events_table}'; \
                 schema version will be read as NULL (treated as v1) for this table: {e}"
            );
        }
        Self {
            postgres,
            bus,
            events_table,
            upcasters: Arc::new(UpcasterRegistry::new()),
        }
    }

    /// Returns the name of the events table this store writes to.
    pub fn events_table(&self) -> &str {
        &self.events_table
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

    /// Stores multiple events within a provided transaction.
    ///
    /// Does NOT publish to event bus - caller is responsible for publishing
    /// after committing the transaction using [`publish_events`](Self::publish_events).
    ///
    /// Returns events with `global_sequence` populated.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut tx = event_store.pool().begin().await?;
    ///
    /// let stored_events = event_store.store_events_in_tx(&mut tx, events).await?;
    /// MyState::upsert(id, &state, &mut *tx).await?;
    ///
    /// tx.commit().await?;
    ///
    /// event_store.publish_events(stored_events).await?;
    /// ```
    pub async fn store_events_in_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        events: Vec<Event<B::EventType>>,
    ) -> Result<Vec<Event<B::EventType>>, PgEventStoreError<B::Error>>
    where
        B::EventType: Serialize,
    {
        let mut stored_events = Vec::with_capacity(events.len());

        let insert_sql = format!(
            "INSERT INTO {} (id, stream_id, stream_version, event_type, data, \
             created_at, actor_id, purger_id, purged_at, causation_id, correlation_id, \
             schema_version) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
             RETURNING global_sequence",
            self.events_table,
        );
        for event in events {
            let row: (i64,) = sqlx::query_as(&insert_sql)
                .bind(event.id)
                .bind(event.stream_id)
                .bind(TryInto::<i64>::try_into(event.stream_version).map_err(|e| {
                    PgEventStoreError::DBError::<B::Error>(sqlx::error::Error::InvalidArgument(
                        format!(
                            "stream_version {} is too large to fit in i64: {}",
                            event.stream_version, e
                        ),
                    ))
                })?)
                .bind(event.event_type.to_string())
                .bind(event.data.as_ref().map(serde_json::to_value).transpose()?)
                .bind(event.created_at)
                .bind(event.actor_id)
                .bind(event.purger_id)
                .bind(event.purged_at)
                .bind(event.causation_id)
                .bind(event.correlation_id)
                .bind(event.schema_version as i32)
                .fetch_one(&mut **tx)
                .await?;

            stored_events.push(Event {
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
                causation_id: event.causation_id,
                correlation_id: event.correlation_id,
                schema_version: event.schema_version,
            });
        }

        Ok(stored_events)
    }

    /// Publishes events to the event bus.
    ///
    /// Call this after committing a transaction that used [`store_events_in_tx`](Self::store_events_in_tx).
    pub async fn publish_events(
        &self,
        events: Vec<Event<B::EventType>>,
    ) -> Result<(), PgEventStoreError<B::Error>>
    where
        B::EventType: Send + Sync,
        B::Error: Send + Sync,
    {
        for event in events {
            self.bus
                .publish(Arc::new(event))
                .await
                .map_err(PgEventStoreError::BUSPublishError)?;
        }
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
    /// The ID of the event that caused this event to be produced.
    pub causation_id: Option<Uuid>,
    /// A shared identifier tying together all events in a causal tree.
    pub correlation_id: Option<Uuid>,
    /// The schema version the payload was stored at. `NULL` on pre-migration rows
    /// (interpreted as version `1` by the read path); stamped explicitly on new inserts.
    #[sqlx(default)]
    pub schema_version: Option<i32>,
}

/// Converts a [`PgDBEvent`] row into a domain [`Event`].
///
/// This is the single source of truth for the `PgDBEvent` → `Event` builder
/// sequence shared by `read_events_since`, `read_events_by_correlation_id`,
/// `trace_causation_chain`, and `read_last_event`. Optional metadata
/// (`global_sequence`, `causation_id`, `correlation_id`) is only threaded onto
/// the builder when present on the row.
async fn pg_db_event_to_event<D, BE>(
    entry: PgDBEvent,
    upcasters: &UpcasterRegistry,
) -> Result<Option<Event<D>>, PgEventStoreError<BE>>
where
    D: EventData + DeserializeOwned,
    BE: std::error::Error,
{
    // The stored schema version. `NULL` (pre-migration rows) is interpreted as
    // version `1` — the universal floor that lets us avoid a backfill.
    let stored_version = entry.schema_version.unwrap_or(1).max(0) as u32;
    let has_payload = entry.data.is_some();

    // Route through the registry: upcast the stored payload forward to the current
    // schema version, then deserialize into `D`, applying the configured
    // `FailurePolicy`. `Ok(None)` is returned only when an event is explicitly
    // dead-lettered (skip that row) or when the payload is `NULL` (purged event).
    let data: Option<D> = upcasters
        .upcast_and_deserialize::<D>(
            &entry.event_type,
            stored_version,
            entry.stream_id,
            entry.id,
            entry.data,
        )
        .await?;

    // If the row carried a payload but the registry returned `None`, the event was
    // explicitly dead-lettered (counted, logged, captured): skip exactly this row.
    if has_payload && data.is_none() {
        return Ok(None);
    }

    let mut builder = Event::<D>::builder()
        .id(entry.id)
        .stream_id(entry.stream_id)
        .stream_version(
            u64::try_from(entry.stream_version)
                .map_err(|_| PgEventStoreError::InvalidStreamVersion::<BE>(entry.stream_version))?,
        )
        .event_type(entry.event_type)
        .created_at(entry.created_at)
        .schema_version(stored_version)
        .data(data);

    // Add global_sequence if present
    if let Some(gs) = entry.global_sequence {
        builder = builder.global_sequence(gs as u64);
    }

    // Add causation/correlation if present
    if let Some(cid) = entry.causation_id {
        builder = builder.causation_id(cid);
    }
    if let Some(cid) = entry.correlation_id {
        builder = builder.correlation_id(cid);
    }

    builder
        .build()
        .map(Some)
        .map_err(PgEventStoreError::BuildEventError::<BE>)
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
    /// A stored `stream_version` is negative and cannot be converted to `u64` (data corruption)
    #[error("Invalid stream_version {0}: value is negative (data corruption)")]
    InvalidStreamVersion(i64),
    /// An upcasting or deserialization failure reported by the [`UpcasterRegistry`].
    ///
    /// Only produced when [`epoch_core::upcasting::FailurePolicy::Fail`] is active (the
    /// default). Under [`epoch_core::upcasting::FailurePolicy::DeadLetter`] the event is
    /// captured by the configured sink and skipped (`Ok(None)`) without propagating an error.
    ///
    /// [`UpcasterRegistry`]: epoch_core::upcasting::UpcasterRegistry
    #[error("Upcast error: {0}")]
    Upcast(#[from] epoch_core::upcasting::UpcastError),
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
        let read_sql = format!(
            "SELECT id, stream_id, stream_version, event_type, data, created_at, \
             actor_id, purger_id, purged_at, global_sequence, causation_id, correlation_id, \
             schema_version \
             FROM {} WHERE stream_id = $1 AND stream_version >= $2 \
             ORDER BY stream_version ASC",
            self.events_table,
        );
        let stream = try_stream! {
            let mut inner_stream = sqlx::query_as::<_, PgDBEvent>(&read_sql)
            .bind(stream_id)
            .bind(version as i64)
            .fetch(&self.postgres);

            while let Some(row) = inner_stream.next().await {
                let entry: PgDBEvent = row.map_err(PgEventStoreError::DBError::<B::Error>)?;

                // `None` means the row was explicitly dead-lettered (counted, logged,
                // captured): skip it without aborting the stream.
                if let Some(event) =
                    pg_db_event_to_event::<B::EventType, B::Error>(entry, &self.upcasters).await?
                {
                    yield event;
                }
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
        let store_sql = format!(
            "INSERT INTO {} (id, stream_id, stream_version, event_type, data, \
             created_at, actor_id, purger_id, purged_at, causation_id, correlation_id, \
             schema_version) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
             RETURNING global_sequence",
            self.events_table,
        );
        let row: (i64,) = sqlx::query_as(&store_sql)
            .bind(event.id)
            .bind(event.stream_id)
            .bind(TryInto::<i64>::try_into(event.stream_version).map_err(|e| {
                PgEventStoreError::DBError(sqlx::error::Error::InvalidArgument(format!(
                    "stream_version {} is too large to fit in i64: {}",
                    event.stream_version, e
                )))
            })?)
            .bind(event.event_type.to_string())
            .bind(event.data.as_ref().map(serde_json::to_value).transpose()?)
            .bind(event.created_at)
            .bind(event.actor_id)
            .bind(event.purger_id)
            .bind(event.purged_at)
            .bind(event.causation_id)
            .bind(event.correlation_id)
            .bind(event.schema_version as i32)
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
            causation_id: event.causation_id,
            correlation_id: event.correlation_id,
            schema_version: event.schema_version,
        };

        // Wrap in Arc for efficient sharing - no clone needed
        self.bus
            .publish(std::sync::Arc::new(event_with_sequence))
            .await
            .map_err(PgEventStoreError::BUSPublishError)?;

        Ok(())
    }

    /// Stores multiple events atomically in a single transaction.
    ///
    /// Events are persisted in a single database transaction, ensuring all-or-nothing
    /// semantics. After the transaction commits, events are published to the event bus.
    ///
    /// # Note
    ///
    /// If event bus publishing fails partway through, the events remain committed to the
    /// database. This is acceptable for event sourcing: events are durable and projections
    /// can catch up by replaying from the event store.
    async fn store_events(&self, events: Vec<Event<Self::EventType>>) -> Result<(), Self::Error> {
        if events.is_empty() {
            return Ok(());
        }

        let mut tx = self.postgres.begin().await?;
        let stored_events = self.store_events_in_tx(&mut tx, events).await?;
        tx.commit().await?;

        self.publish_events(stored_events).await
    }

    /// Returns the most recent event of the given stream via a single indexed query.
    ///
    /// Overrides the default O(N) trait implementation with an
    /// `ORDER BY stream_version DESC LIMIT 1` query served by the
    /// `UNIQUE (stream_id, stream_version)` index, fetching at most one row.
    /// Returns `Ok(None)` when the stream is empty or does not exist.
    async fn read_last_event(
        &self,
        stream_id: Uuid,
    ) -> Result<Option<Event<Self::EventType>>, Self::Error> {
        // Fetch rows in descending version order without a LIMIT so that, when the
        // `DeadLetter` policy is active, we can skip dead-lettered rows at the tail
        // and return the most-recent *live* event instead of falsely reporting an
        // empty stream.  In the common (no-dead-letter) case the loop exits on the
        // very first row.
        let last_sql = format!(
            "SELECT id, stream_id, stream_version, event_type, data, created_at, \
             actor_id, purger_id, purged_at, global_sequence, causation_id, correlation_id, \
             schema_version \
             FROM {} WHERE stream_id = $1 ORDER BY stream_version DESC",
            self.events_table,
        );
        let mut rows = sqlx::query_as::<_, PgDBEvent>(&last_sql)
            .bind(stream_id)
            .fetch(&self.postgres);

        use futures_util::StreamExt;
        while let Some(row) = rows.next().await {
            let entry = row.map_err(PgEventStoreError::DBError::<B::Error>)?;
            if let Some(event) =
                pg_db_event_to_event::<B::EventType, B::Error>(entry, &self.upcasters).await?
            {
                return Ok(Some(event));
            }
            // `Ok(None)` means the row was dead-lettered; advance to the prior row.
        }
        Ok(None)
    }

    async fn read_events_by_correlation_id(
        &self,
        correlation_id: Uuid,
    ) -> Result<Vec<Event<Self::EventType>>, Self::Error> {
        let correlation_sql = format!(
            "SELECT id, stream_id, stream_version, event_type, data, created_at, \
             actor_id, purger_id, purged_at, global_sequence, causation_id, correlation_id, \
             schema_version \
             FROM {} WHERE correlation_id = $1 ORDER BY global_sequence ASC",
            self.events_table,
        );
        let rows = sqlx::query_as::<_, PgDBEvent>(&correlation_sql)
            .bind(correlation_id)
            .fetch_all(&self.postgres)
            .await?;

        let mut events = Vec::with_capacity(rows.len());
        for entry in rows {
            // Skip dead-lettered rows (`None`); fail loudly otherwise.
            if let Some(event) =
                pg_db_event_to_event::<B::EventType, B::Error>(entry, &self.upcasters).await?
            {
                events.push(event);
            }
        }

        Ok(events)
    }

    async fn trace_causation_chain(
        &self,
        event_id: Uuid,
    ) -> Result<Vec<Event<Self::EventType>>, Self::Error> {
        // Fetch the starting event
        let trace_sql = format!(
            "SELECT id, stream_id, stream_version, event_type, data, created_at, \
             actor_id, purger_id, purged_at, global_sequence, causation_id, correlation_id, \
             schema_version \
             FROM {} WHERE id = $1",
            self.events_table,
        );
        let row = sqlx::query_as::<_, PgDBEvent>(&trace_sql)
            .bind(event_id)
            .fetch_optional(&self.postgres)
            .await?;

        let entry = match row {
            Some(entry) => entry,
            None => return Ok(vec![]),
        };

        // If no correlation_id, return just this event
        let correlation_id = match entry.correlation_id {
            Some(cid) => cid,
            None => {
                let events = pg_db_event_to_event::<B::EventType, B::Error>(entry, &self.upcasters)
                    .await?
                    .into_iter()
                    .collect();
                return Ok(events);
            }
        };

        // Get all correlated events and extract the subtree
        let correlated_events = self.read_events_by_correlation_id(correlation_id).await?;
        Ok(epoch_core::causation::extract_causation_subtree(
            correlated_events,
            event_id,
        ))
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
            causation_id: None,
            correlation_id: None,
            schema_version: None,
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
            causation_id: None,
            correlation_id: None,
            schema_version: None,
        };

        let json = serde_json::to_string(&db_event).unwrap();
        let parsed: PgDBEvent = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.global_sequence, None);
    }

    #[tokio::test]
    async fn pg_db_event_to_event_rejects_negative_stream_version() {
        // A corrupt row with a negative stream_version must produce a typed error,
        // not a panic. Regression test for CLOUD-170.
        #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
        struct TestEvent;
        impl epoch_core::event::EventData for TestEvent {
            fn event_type(&self) -> &'static str {
                "TestEvent"
            }
        }

        let entry = PgDBEvent {
            id: Uuid::new_v4(),
            stream_id: Uuid::new_v4(),
            stream_version: -1,
            actor_id: None,
            event_type: "TestEvent".to_string(),
            data: None,
            created_at: chrono::Utc::now(),
            purger_id: None,
            purged_at: None,
            global_sequence: None,
            causation_id: None,
            correlation_id: None,
            schema_version: None,
        };

        let registry = UpcasterRegistry::new();
        let result =
            pg_db_event_to_event::<TestEvent, std::convert::Infallible>(entry, &registry).await;
        assert!(
            matches!(result, Err(PgEventStoreError::InvalidStreamVersion(-1))),
            "expected InvalidStreamVersion(-1), got: {:?}",
            result
        );
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
