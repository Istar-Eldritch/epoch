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

/// Raises the `epoch_events_sequence_counter` row for `events_table` to the
/// table's current high-water mark, creating it if absent.
///
/// The floor combines the sequence's last-*assigned* value (`is_called = false`
/// means a virgin sequence has assigned nothing) with the largest committed
/// `global_sequence`, so the first counter-drawn value cannot collide with a
/// value `nextval` already handed out.
///
/// The read and the insert are separate statements on the pool; the
/// [`AllocationMode::PerTxnCounter`] opt-in contract (spec 0030 §3.6 / OQ-4)
/// makes a quiesced writer set the operator's obligation, and the
/// `GREATEST`-on-conflict keeps the row monotone even if a straggler
/// `nextval` writer interleaves — a higher value drawn after the read still
/// wins on conflict, so this never regresses.
async fn reseed_sequence_counter(postgres: &PgPool, events_table: &str) -> Result<(), sqlx::Error> {
    let sequence: Option<String> =
        sqlx::query_scalar("SELECT pg_get_serial_sequence($1, 'global_sequence')")
            .bind(events_table)
            .fetch_one(postgres)
            .await?;

    let sequence_last_assigned = match sequence {
        // pg_get_serial_sequence returns an already-quoted, schema-qualified name.
        Some(sequence) => {
            let (last_value, is_called): (i64, bool) =
                sqlx::query_as(&format!("SELECT last_value, is_called FROM {sequence}"))
                    .fetch_one(postgres)
                    .await?;
            if is_called {
                last_value
            } else {
                last_value - 1
            }
        }
        None => 0,
    };

    let max_global_sequence: Option<i64> =
        sqlx::query_scalar(&format!("SELECT MAX(global_sequence) FROM {events_table}"))
            .fetch_one(postgres)
            .await?;

    let floor = sequence_last_assigned.max(max_global_sequence.unwrap_or(0));

    sqlx::query(
        "INSERT INTO epoch_events_sequence_counter (name, val) VALUES ($1, $2) \
         ON CONFLICT (name) DO UPDATE \
         SET val = GREATEST(epoch_events_sequence_counter.val, EXCLUDED.val)",
    )
    .bind(events_table)
    .bind(floor)
    .execute(postgres)
    .await?;

    Ok(())
}

/// Draws `k` `global_sequence` values for `events_table` from the counter row,
/// inside the caller's transaction, returning the **first** value of the block
/// (the block is `first ..= first + k - 1`).
///
/// The draw is an ordinary row UPDATE, so it takes the counter row's lock for
/// the remainder of the transaction: concurrent writers serialize here, and a
/// rollback or crash rewinds the draw (zero burn).
///
/// # Failure
///
/// If the counter row for `events_table` is absent (the table was dropped, or
/// migration m014 never ran) the UPDATE matches no row and this returns
/// [`sqlx::Error::RowNotFound`], failing the insert. There is deliberately no
/// fallback to `nextval`: a silent fallback would hand out values the counter
/// has already promised.
async fn draw_sequence_block(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    events_table: &str,
    k: i64,
) -> Result<i64, sqlx::Error> {
    let last: i64 = sqlx::query_scalar(
        "UPDATE epoch_events_sequence_counter SET val = val + $2 WHERE name = $1 RETURNING val",
    )
    .bind(events_table)
    .bind(k)
    .fetch_one(&mut **tx)
    .await?;
    Ok(last - k + 1)
}

/// How `global_sequence` values are allocated on the insert path.
///
/// Selected once at [`PgEventStore`] construction: this is a deployment-wide
/// choice, not a per-writer or per-subscriber one. The default
/// ([`AllocationMode::Nextval`]) is today's path, unchanged; the mere existence
/// of this enum costs a default-mode writer nothing.
///
/// # The counter row is keyed by events-table name
///
/// Under [`AllocationMode::PerTxnCounter`] the allocator draws from a row of
/// `epoch_events_sequence_counter` (migration m014) keyed by the store's events
/// table, because that table is configurable (see
/// [`with_table`](PgEventStore::with_table)). Two stores on two tables have two
/// independent counters, exactly as they have two independent sequences.
///
/// # Opting in is a one-way, deployment-lifetime choice
///
/// `nextval` is non-transactional, so a `Nextval` writer running concurrently
/// with a `PerTxnCounter` writer can hand out a value the counter has already
/// promised. The transition therefore requires, in order:
///
/// 1. **Quiesce the writers.** No `Nextval` writer may be running.
/// 2. **Construct under `PerTxnCounter`** via
///    [`with_allocation_mode`](PgEventStore::with_allocation_mode), which
///    re-seeds the counter row above the current high-water mark and fails
///    construction if it cannot.
///
/// Switching back is not a built path. It remains a documented operator
/// procedure: quiesce writers, `setval` the events table's sequence past the
/// counter row's `val`, then restart under `Nextval`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum AllocationMode {
    /// The column `DEFAULT nextval(...)` assigns `global_sequence` — byte-for-byte
    /// today's path, and the default.
    ///
    /// The INSERT omits the `global_sequence` column entirely, the DEFAULT
    /// fires, and `RETURNING global_sequence` reads the assigned value back. No
    /// counter row is read, written, or required.
    ///
    /// `nextval` is deliberately non-transactional, which is what makes it
    /// cheap: it never blocks a concurrent writer. The cost is that a
    /// rolled-back or crashed transaction **permanently burns** the values it
    /// drew, leaving a hole in `global_sequence` that no row will ever fill.
    #[default]
    Nextval,
    /// Each insert transaction draws its `global_sequence` values from the
    /// per-events-table counter row **inside that same transaction** — zero burn.
    ///
    /// Because the draw is an ordinary row UPDATE, a rollback rewinds the
    /// counter and a crash aborts it: neither burns a value, so the committed
    /// sequence is contiguous.
    ///
    /// # Write-tax trade-off
    ///
    /// The counter row serializes every writer on the events table. Measured at
    /// **3.5–4.4× slower than `nextval` for single-event transactions** (K=1);
    /// the cost is per transaction, not per event, so a multi-event aggregate
    /// transaction amortizes it across its K events. This is why `Nextval`
    /// remains the default: pay this tax only if contiguous sequences are worth
    /// more to you than write throughput.
    ///
    /// # Fence-safe by construction
    ///
    /// That same serialization is what keeps the gap/fence machinery valid with
    /// no change: the counter lock makes sequence order equal commit order, so
    /// a committed event can never appear above a hole, and a missing
    /// sequence's writer (if any) necessarily holds an xid inside the
    /// transaction that drew the value — precisely the premise the snapshot
    /// fence already assumes under `nextval`.
    ///
    /// # Opting in
    ///
    /// Requires migration m014 and the fallible constructor
    /// [`with_allocation_mode`](PgEventStore::with_allocation_mode) with writers
    /// quiesced; see the type-level docs for the full one-way transition
    /// contract.
    PerTxnCounter,
}

/// A postgres based event store.
///
#[derive(Clone)]
pub struct PgEventStore<B: EventBus + Clone> {
    postgres: PgPool,
    bus: B,
    events_table: String,
    upcasters: Arc<UpcasterRegistry>,
    allocation_mode: AllocationMode,
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
            allocation_mode: AllocationMode::default(),
        }
    }

    /// Creates a new `PgEventStore` writing to the default `epoch_events` table
    /// with a configured [`UpcasterRegistry`].
    ///
    /// The registry is consulted on every read at the deserialization boundary
    /// (`pg_db_event_to_event`): stored payloads are upcast forward to the
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
            allocation_mode: AllocationMode::default(),
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
            allocation_mode: AllocationMode::default(),
        }
    }

    /// Creates a new `PgEventStore` with an explicit [`AllocationMode`].
    ///
    /// Unlike [`with_table`](Self::with_table) this constructor is fallible:
    /// under [`AllocationMode::PerTxnCounter`] it re-seeds the per-table counter
    /// row to `GREATEST(existing counter, sequence last-assigned, MAX(global_sequence))`,
    /// creating the row if absent, and a failure to do so fails construction —
    /// a silently-failed re-seed would let the allocator hand out values that
    /// collide with existing rows.
    ///
    /// The re-seed is idempotent and runs on every construction. Transitioning a
    /// live deployment from `Nextval` to `PerTxnCounter` additionally requires
    /// quiescing writers, since `nextval` is non-transactional and would keep
    /// advancing past the seed.
    ///
    /// Requires migration m014 (`epoch_events_sequence_counter`).
    pub async fn with_allocation_mode(
        postgres: PgPool,
        bus: B,
        events_table: impl Into<String>,
        allocation_mode: AllocationMode,
    ) -> Result<Self, PgEventStoreError<B::Error>> {
        Self::with_allocation_mode_and_upcasters(
            postgres,
            bus,
            events_table,
            allocation_mode,
            Arc::new(UpcasterRegistry::new()),
        )
        .await
    }

    /// Like [`with_allocation_mode`](Self::with_allocation_mode), but also
    /// installs an [`UpcasterRegistry`] — the two options are independent, so a
    /// `PerTxnCounter` store can still upcast on read (see
    /// [`with_upcasters`](Self::with_upcasters) for the registry's role).
    pub async fn with_allocation_mode_and_upcasters(
        postgres: PgPool,
        bus: B,
        events_table: impl Into<String>,
        allocation_mode: AllocationMode,
        upcasters: Arc<UpcasterRegistry>,
    ) -> Result<Self, PgEventStoreError<B::Error>> {
        let events_table = events_table.into();
        log::debug!(
            "Creating a new PgEventStore targeting table '{events_table}' with {allocation_mode:?} allocation"
        );
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
        if allocation_mode == AllocationMode::PerTxnCounter {
            reseed_sequence_counter(&postgres, &events_table).await?;
        }
        Ok(Self {
            postgres,
            bus,
            events_table,
            upcasters,
            allocation_mode,
        })
    }

    /// Returns the name of the events table this store writes to.
    pub fn events_table(&self) -> &str {
        &self.events_table
    }

    /// Returns the `global_sequence` allocation mode this store writes under.
    pub fn allocation_mode(&self) -> AllocationMode {
        self.allocation_mode
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
    /// # Allocation
    ///
    /// Under the default [`AllocationMode::Nextval`] the INSERT omits
    /// `global_sequence`, the column DEFAULT `nextval(...)` assigns it, and a
    /// rollback or crash permanently burns the values drawn.
    ///
    /// Under [`AllocationMode::PerTxnCounter`] the whole batch draws `+K` once
    /// from this table's counter row inside `tx` (serializing writers on that
    /// row) and the values are supplied explicitly, so a rollback or crash
    /// burns nothing. If the counter row is missing the insert fails with
    /// [`sqlx::Error::RowNotFound`] — there is no fallback to `nextval`.
    /// Either way `RETURNING global_sequence` remains the read-back contract.
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

        // PerTxnCounter draws the whole block once, inside the caller's txn.
        let mut next_sequence = match self.allocation_mode {
            AllocationMode::Nextval => None,
            AllocationMode::PerTxnCounter if events.is_empty() => None,
            AllocationMode::PerTxnCounter => {
                Some(draw_sequence_block(tx, &self.events_table, events.len() as i64).await?)
            }
        };

        let insert_sql = match self.allocation_mode {
            AllocationMode::Nextval => format!(
                "INSERT INTO {} (id, stream_id, stream_version, event_type, data, \
                 created_at, actor_id, purger_id, purged_at, causation_id, correlation_id, \
                 schema_version) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
                 RETURNING global_sequence",
                self.events_table,
            ),
            AllocationMode::PerTxnCounter => format!(
                "INSERT INTO {} (id, stream_id, stream_version, event_type, data, \
                 created_at, actor_id, purger_id, purged_at, causation_id, correlation_id, \
                 schema_version, global_sequence) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) \
                 RETURNING global_sequence",
                self.events_table,
            ),
        };
        for event in events {
            let mut query = sqlx::query_as(&insert_sql)
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
                .bind(event.schema_version as i32);
            if let Some(sequence) = next_sequence {
                query = query.bind(sequence);
                next_sequence = Some(sequence + 1);
            }
            let row: (i64,) = query.fetch_one(&mut **tx).await?;

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
        .data(data)
        .schema_version(stored_version);

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

    async fn read_events_range(
        &self,
        stream_id: Uuid,
        from: Option<u64>,
        to: Option<u64>,
    ) -> Result<Pin<Box<dyn EventStream<Self::EventType, Self::Error> + Send + 'life0>>, Self::Error>
    {
        let mut where_clause = String::from("stream_id = $1");
        let mut next_param: u32 = 2;
        if from.is_some() {
            where_clause.push_str(&format!(" AND stream_version >= ${next_param}"));
            next_param += 1;
        }
        if to.is_some() {
            where_clause.push_str(&format!(" AND stream_version <= ${next_param}"));
        }

        let read_sql = format!(
            "SELECT id, stream_id, stream_version, event_type, data, created_at, \
             actor_id, purger_id, purged_at, global_sequence, causation_id, correlation_id, \
             schema_version \
             FROM {} WHERE {} \
             ORDER BY stream_version ASC",
            self.events_table, where_clause,
        );

        let stream = try_stream! {
            let mut query = sqlx::query_as::<_, PgDBEvent>(&read_sql).bind(stream_id);
            if let Some(v) = from {
                query = query.bind(v as i64);
            }
            if let Some(v) = to {
                query = query.bind(v as i64);
            }
            let mut inner_stream = query.fetch(&self.postgres);

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

    /// Stores a single event and publishes it.
    ///
    /// # Allocation
    ///
    /// Under the default [`AllocationMode::Nextval`] the INSERT omits
    /// `global_sequence` and runs directly on the pool: the column DEFAULT
    /// assigns the value and a failed insert burns it.
    ///
    /// Under [`AllocationMode::PerTxnCounter`] the insert runs in its own
    /// transaction that first draws `+1` from this table's counter row and then
    /// supplies the value explicitly, so a rollback or crash burns nothing;
    /// writers serialize on the counter row. A missing counter row fails the
    /// insert with [`sqlx::Error::RowNotFound`] — never a fallback to `nextval`.
    /// `RETURNING global_sequence` remains the read-back contract in both modes.
    async fn store_event(&self, event: Event<Self::EventType>) -> Result<(), Self::Error> {
        if self.allocation_mode == AllocationMode::PerTxnCounter {
            let mut tx = self.postgres.begin().await?;
            let stored = self.store_events_in_tx(&mut tx, vec![event]).await?;
            tx.commit().await?;
            return self.publish_events(stored).await;
        }

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

    /// Persists `events` in a single transaction and returns the enriched
    /// (`global_sequence`-stamped) events WITHOUT publishing them.
    ///
    /// This is the persist half of the fused [`store_events`](Self::store_events),
    /// sharing the same [`store_events_in_tx`](Self::store_events_in_tx) +
    /// `tx.commit()` sequence. Pair with
    /// [`publish_stored_events`](Self::publish_stored_events) to publish afterward.
    async fn store_events_without_publish(
        &self,
        events: Vec<Event<Self::EventType>>,
    ) -> Result<Vec<Event<Self::EventType>>, Self::Error> {
        if events.is_empty() {
            return Ok(Vec::new());
        }

        let mut tx = self.postgres.begin().await?;
        let stored_events = self.store_events_in_tx(&mut tx, events).await?;
        tx.commit().await?;

        Ok(stored_events)
    }

    /// Publishes already-durable `events` to the bus. Pairs with
    /// [`store_events_without_publish`](Self::store_events_without_publish).
    async fn publish_stored_events(
        &self,
        events: Vec<Event<Self::EventType>>,
    ) -> Result<(), Self::Error> {
        self.publish_events(events).await
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
