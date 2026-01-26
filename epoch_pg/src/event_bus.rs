//! This module defines the `PgEventBus` that implements epoch_core::EventBus using PostgreSQL's
//! LISTEN/NOTIFY feature.
use crate::event_store::PgDBEvent;
use epoch_core::event::{Event, EventData};
use epoch_core::event_store::EventBus;
use epoch_core::prelude::EventObserver;
use log::{error, info, warn};
use serde::de::DeserializeOwned;
use sqlx::Error as SqlxError;
use sqlx::postgres::{PgListener, PgPool};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant, sleep};
use uuid::Uuid;

use std::future::Future;

/// Configuration for reliable event delivery.
///
/// This struct controls retry behavior, checkpointing strategy, and multi-instance coordination.
#[derive(Debug, Clone)]
pub struct ReliableDeliveryConfig {
    /// Maximum number of retry attempts for failed event processing.
    /// After this many failures, the event is sent to the dead letter queue.
    pub max_retries: u32,

    /// Initial delay before the first retry attempt.
    /// Subsequent retries use exponential backoff.
    pub initial_retry_delay: Duration,

    /// Maximum delay between retry attempts.
    /// Exponential backoff is capped at this value.
    pub max_retry_delay: Duration,

    /// How checkpoints are persisted.
    pub checkpoint_mode: CheckpointMode,

    /// How multiple instances coordinate event processing.
    ///
    /// Currently only `SingleInstance` is supported. For horizontal scaling
    /// scenarios, see the advisory lock helper methods which can be used
    /// for manual coordination.
    pub instance_mode: InstanceMode,

    /// Number of events to process in each catch-up batch.
    pub catch_up_batch_size: u32,
}

impl Default for ReliableDeliveryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_retry_delay: Duration::from_secs(1),
            max_retry_delay: Duration::from_secs(60),
            checkpoint_mode: CheckpointMode::Synchronous,
            instance_mode: InstanceMode::SingleInstance,
            catch_up_batch_size: 100,
        }
    }
}

/// Determines how checkpoints are persisted after event processing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum CheckpointMode {
    /// Checkpoint is written immediately after each successful event processing.
    /// Provides strongest durability guarantees but may impact throughput.
    /// On crash, at most 1 event may be redelivered.
    #[default]
    Synchronous,

    /// Batch checkpoint updates for better performance.
    ///
    /// Checkpoints are written when either:
    /// - `batch_size` events have been processed since the last checkpoint, OR
    /// - `max_delay_ms` milliseconds have elapsed since the first unacknowledged event
    ///
    /// This trades off a window of potential duplicate deliveries on crash
    /// for significantly improved throughput. On crash, up to `batch_size` events
    /// may be redelivered.
    ///
    /// **Use when:**
    /// - High event throughput (1000+ events/second)
    /// - Projections are idempotent
    /// - Catch-up performance is critical
    Batched {
        /// Number of events to process before updating checkpoint.
        batch_size: u32,
        /// Maximum time (in milliseconds) before forcing a checkpoint update.
        /// Prevents unbounded delay when event rate is low.
        max_delay_ms: u64,
    },
}

impl CheckpointMode {
    /// Creates a new `Batched` checkpoint mode with the given settings.
    ///
    /// # Arguments
    /// * `batch_size` - Number of events to process before checkpointing
    /// * `max_delay` - Maximum time before forcing a checkpoint
    ///
    /// # Example
    /// ```
    /// use epoch_pg::CheckpointMode;
    /// use std::time::Duration;
    ///
    /// let mode = CheckpointMode::batched(100, Duration::from_secs(5));
    /// ```
    pub fn batched(batch_size: u32, max_delay: Duration) -> Self {
        Self::Batched {
            batch_size,
            max_delay_ms: max_delay.as_millis() as u64,
        }
    }

    /// Creates a `Batched` checkpoint mode with default settings.
    ///
    /// Defaults: `batch_size = 100`, `max_delay = 5 seconds`
    pub fn batched_default() -> Self {
        Self::Batched {
            batch_size: 100,
            max_delay_ms: 5000,
        }
    }
}

/// Determines how multiple instances of the same subscriber coordinate.
///
/// For multi-instance deployments, use `Coordinated` mode to ensure only one
/// instance processes events for each subscriber. For single-instance deployments
/// or external orchestration, use `SingleInstance` mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum InstanceMode {
    /// No coordination between instances. Use when running a single instance
    /// or when external orchestration handles instance management.
    #[default]
    SingleInstance,

    /// Use PostgreSQL advisory locks to ensure only one instance
    /// processes events for each subscriber. Recommended for multi-instance
    /// deployments. Provides automatic failover when an instance dies.
    ///
    /// When enabled:
    /// - `subscribe()` attempts to acquire a lock before catch-up
    /// - If lock is not acquired, the subscribe call succeeds but the projection
    ///   is not registered (another instance is processing)
    /// - Locks are automatically released when the database connection closes
    Coordinated,
}

/// Result of processing an event with retry logic.
#[derive(Debug)]
enum ProcessResult {
    /// Event was processed successfully
    Success,
    /// Event failed after all retries and was sent to DLQ
    SentToDlq,
}

/// Tracks pending checkpoint state for batched checkpointing.
///
/// This struct holds the checkpoint data that hasn't been flushed to the database yet.
#[derive(Debug, Clone)]
struct PendingCheckpoint {
    /// The global sequence to checkpoint
    global_sequence: u64,
    /// The event ID to checkpoint
    event_id: Uuid,
    /// Number of events processed since last checkpoint write
    events_since_checkpoint: u32,
    /// When the first event was processed since last checkpoint write
    first_event_time: Instant,
}

impl PendingCheckpoint {
    /// Creates a new pending checkpoint.
    fn new(global_sequence: u64, event_id: Uuid) -> Self {
        Self {
            global_sequence,
            event_id,
            events_since_checkpoint: 1,
            first_event_time: Instant::now(),
        }
    }

    /// Updates the pending checkpoint with a new event.
    fn update(&mut self, global_sequence: u64, event_id: Uuid) {
        self.global_sequence = global_sequence;
        self.event_id = event_id;
        self.events_since_checkpoint += 1;
    }
}

/// Determines whether a pending checkpoint should be flushed to the database.
///
/// For `Synchronous` mode, always returns `true`.
/// For `Batched` mode, returns `true` if either:
/// - The number of events since last flush >= `batch_size`
/// - The time since the first unacknowledged event >= `max_delay_ms`
fn should_flush_checkpoint(pending: &PendingCheckpoint, mode: &CheckpointMode) -> bool {
    match mode {
        CheckpointMode::Synchronous => true,
        CheckpointMode::Batched {
            batch_size,
            max_delay_ms,
        } => {
            let max_delay = Duration::from_millis(*max_delay_ms);
            pending.events_since_checkpoint >= *batch_size
                || pending.first_event_time.elapsed() >= max_delay
        }
    }
}

/// Flushes a pending checkpoint to the database.
///
/// This writes the checkpoint and updates the in-memory cache.
async fn flush_checkpoint(
    pool: &PgPool,
    subscriber_id: &str,
    pending: &PendingCheckpoint,
    checkpoint_cache: &mut HashMap<String, u64>,
) {
    if let Err(e) = sqlx::query(
        r#"
        INSERT INTO epoch_event_bus_checkpoints (subscriber_id, last_global_sequence, last_event_id, updated_at)
        VALUES ($1, $2, $3, NOW())
        ON CONFLICT (subscriber_id) DO UPDATE SET
            last_global_sequence = EXCLUDED.last_global_sequence,
            last_event_id = EXCLUDED.last_event_id,
            updated_at = NOW()
        "#,
    )
    .bind(subscriber_id)
    .bind(pending.global_sequence as i64)
    .bind(pending.event_id)
    .execute(pool)
    .await
    {
        error!(
            "Failed to flush checkpoint for '{}': {}",
            subscriber_id, e
        );
    } else {
        checkpoint_cache.insert(subscriber_id.to_string(), pending.global_sequence);
        log::debug!(
            "Flushed checkpoint for '{}' to global_sequence {} ({} events batched)",
            subscriber_id,
            pending.global_sequence,
            pending.events_since_checkpoint
        );
    }
}

/// Flushes all pending checkpoints that have exceeded their max_delay.
///
/// This is called periodically to ensure checkpoints are written even when
/// event rate is low.
async fn flush_expired_checkpoints(
    pool: &PgPool,
    pending_checkpoints: &mut HashMap<String, PendingCheckpoint>,
    checkpoint_cache: &mut HashMap<String, u64>,
    mode: &CheckpointMode,
) {
    let CheckpointMode::Batched { max_delay_ms, .. } = mode else {
        return; // Nothing to do for Synchronous mode
    };

    let max_delay = Duration::from_millis(*max_delay_ms);
    let expired: Vec<String> = pending_checkpoints
        .iter()
        .filter(|(_, p)| p.first_event_time.elapsed() >= max_delay)
        .map(|(k, _)| k.clone())
        .collect();

    for subscriber_id in expired {
        if let Some(pending) = pending_checkpoints.remove(&subscriber_id) {
            flush_checkpoint(pool, &subscriber_id, &pending, checkpoint_cache).await;
        }
    }
}

/// Flushes all pending checkpoints to the database.
///
/// This is called before reconnection or shutdown to ensure no checkpoint data is lost.
async fn flush_all_pending_checkpoints(
    pool: &PgPool,
    pending_checkpoints: &mut HashMap<String, PendingCheckpoint>,
    checkpoint_cache: &mut HashMap<String, u64>,
) {
    let subscriber_ids: Vec<String> = pending_checkpoints.keys().cloned().collect();
    for subscriber_id in subscriber_ids {
        if let Some(pending) = pending_checkpoints.remove(&subscriber_id) {
            flush_checkpoint(pool, &subscriber_id, &pending, checkpoint_cache).await;
        }
    }
}

/// Calculates the retry delay using exponential backoff.
///
/// The delay doubles with each attempt, starting from `initial_retry_delay`,
/// and is capped at `max_retry_delay`.
///
/// # Arguments
///
/// * `config` - The reliability configuration containing delay settings
/// * `attempt` - The attempt number (0-indexed)
///
/// # Returns
///
/// The delay duration before the next retry attempt.
pub fn calculate_retry_delay(config: &ReliableDeliveryConfig, attempt: u32) -> Duration {
    let initial_ms = config.initial_retry_delay.as_millis() as u64;
    let max_ms = config.max_retry_delay.as_millis() as u64;

    // Use saturating operations to prevent overflow
    // Cap attempt at 63 to prevent 2^attempt from overflowing u64
    let capped_attempt = attempt.min(63);
    let multiplier = 2u64.saturating_pow(capped_attempt);
    let delay_ms = initial_ms.saturating_mul(multiplier);

    Duration::from_millis(delay_ms.min(max_ms))
}

/// Processes an event with retry logic and DLQ fallback.
///
/// This is a helper function used by both real-time event processing and catch-up.
/// It attempts to process the event, retrying with exponential backoff on failure,
/// and inserts into the DLQ if all retries are exhausted.
///
/// Returns whether the event was processed successfully or sent to DLQ.
async fn process_event_with_retry<ED>(
    observer: &Arc<Mutex<dyn EventObserver<ED>>>,
    event: &Arc<Event<ED>>,
    subscriber_id: &str,
    config: &ReliableDeliveryConfig,
    dlq_pool: &PgPool,
) -> ProcessResult
where
    ED: EventData + Send + Sync,
{
    let event_id = event.id;
    let event_global_seq = event.global_sequence.unwrap_or(0);
    let mut last_error: Option<String> = None;

    for attempt in 0..=config.max_retries {
        let observer_guard = observer.lock().await;
        match observer_guard.on_event(Arc::clone(event)).await {
            Ok(_) => {
                log::debug!(
                    "Successfully applied event to '{}': {:?}{}",
                    subscriber_id,
                    event_id,
                    if attempt > 0 {
                        format!(" (after {} retries)", attempt)
                    } else {
                        String::new()
                    }
                );
                return ProcessResult::Success;
            }
            Err(e) => {
                last_error = Some(format!("{:?}", e));
                if attempt < config.max_retries {
                    let delay = calculate_retry_delay(config, attempt);
                    warn!(
                        "Failed applying event {} to '{}' (attempt {}/{}): {:?}. Retrying in {:?}",
                        event_id,
                        subscriber_id,
                        attempt + 1,
                        config.max_retries + 1,
                        e,
                        delay
                    );
                    drop(observer_guard);
                    sleep(delay).await;
                } else {
                    error!(
                        "Failed applying event {} to '{}' after {} attempts: {:?}. Sending to DLQ.",
                        event_id,
                        subscriber_id,
                        config.max_retries + 1,
                        e
                    );
                }
            }
        }
    }

    // All retries exhausted - insert into DLQ
    let error_message = last_error.unwrap_or_else(|| "Unknown error".to_string());
    if let Err(e) = sqlx::query(
        r#"
        INSERT INTO epoch_event_bus_dlq (subscriber_id, event_id, global_sequence, error_message, retry_count, last_retry_at)
        VALUES ($1, $2, $3, $4, $5, NOW())
        ON CONFLICT (subscriber_id, event_id) DO UPDATE SET
            error_message = EXCLUDED.error_message,
            retry_count = EXCLUDED.retry_count,
            last_retry_at = NOW()
        "#,
    )
    .bind(subscriber_id)
    .bind(event_id)
    .bind(event_global_seq as i64)
    .bind(&error_message)
    .bind((config.max_retries + 1) as i32)
    .execute(dlq_pool)
    .await
    {
        error!(
            "Failed to insert event {} into DLQ for '{}': {}",
            event_id, subscriber_id, e
        );
    } else {
        info!(
            "Event {} for '{}' inserted into DLQ after {} failed attempts",
            event_id,
            subscriber_id,
            config.max_retries + 1
        );
    }

    ProcessResult::SentToDlq
}

/// Represents an entry in the dead letter queue.
#[derive(Debug, Clone)]
pub struct DlqEntry {
    /// Unique identifier for the DLQ entry
    pub id: Uuid,
    /// The subscriber that failed to process the event
    pub subscriber_id: String,
    /// The event ID that failed
    pub event_id: Uuid,
    /// The global sequence of the failed event
    pub global_sequence: u64,
    /// Error message from the last failure
    pub error_message: Option<String>,
    /// Number of retry attempts made
    pub retry_count: i32,
    /// When the entry was created
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// When the last retry was attempted
    pub last_retry_at: Option<chrono::DateTime<chrono::Utc>>,
}

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
    config: ReliableDeliveryConfig,
}

impl<D> PgEventBus<D>
where
    D: EventData + Send + Sync + DeserializeOwned + 'static,
{
    /// Creates a new `PgEventBus` instance with default configuration.
    pub fn new(pool: PgPool, channel_name: impl Into<String>) -> Self {
        Self::with_config(pool, channel_name, ReliableDeliveryConfig::default())
    }

    /// Creates a new `PgEventBus` instance with custom configuration.
    pub fn with_config(
        pool: PgPool,
        channel_name: impl Into<String>,
        config: ReliableDeliveryConfig,
    ) -> Self {
        Self {
            pool,
            channel_name: channel_name.into(),
            projections: Arc::new(Mutex::new(vec![])),
            config,
        }
    }

    /// Returns a reference to the configuration.
    pub fn config(&self) -> &ReliableDeliveryConfig {
        &self.config
    }

    /// Returns a reference to the connection pool.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Reads all events across all streams since a given global sequence.
    ///
    /// This is used for catch-up processing when a subscriber needs to replay
    /// events it may have missed. Events are returned ordered by global_sequence.
    ///
    /// # Arguments
    ///
    /// * `since_global_sequence` - The global sequence to start reading from (exclusive).
    ///   Pass 0 to read all events from the beginning.
    /// * `limit` - Maximum number of events to return (for batching).
    ///
    /// # Returns
    ///
    /// A vector of events ordered by global_sequence.
    pub async fn read_all_events_since(
        &self,
        since_global_sequence: u64,
        limit: u32,
    ) -> Result<Vec<Event<D>>, SqlxError> {
        let rows: Vec<PgDBEvent> = sqlx::query_as(
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
            WHERE global_sequence > $1
            ORDER BY global_sequence ASC
            LIMIT $2
            "#,
        )
        .bind(since_global_sequence as i64)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let data: Option<D> = row
                .data
                .map(|d| serde_json::from_value(d))
                .transpose()
                .map_err(|e| {
                    error!("Failed to deserialize event data: {}", e);
                    SqlxError::Decode(Box::new(e))
                })?;

            events.push(Event {
                id: row.id,
                stream_id: row.stream_id,
                stream_version: row.stream_version as u64,
                event_type: row.event_type,
                actor_id: row.actor_id,
                purger_id: row.purger_id,
                data,
                created_at: row.created_at,
                purged_at: row.purged_at,
                global_sequence: row.global_sequence.map(|gs| gs as u64),
            });
        }

        Ok(events)
    }

    /// Sets up the channel-specific trigger for event notifications.
    ///
    /// This method creates or replaces the trigger that sends NOTIFY messages
    /// when events are inserted. The trigger uses this event bus's channel name.
    ///
    /// # Usage
    ///
    /// Call this after running migrations:
    /// ```rust,ignore
    /// use epoch_pg::{Migrator, PgEventBus};
    ///
    /// // Run migrations first
    /// Migrator::new(pool.clone()).run().await?;
    ///
    /// // Then set up the channel-specific trigger
    /// let event_bus = PgEventBus::new(pool.clone(), "my_channel");
    /// event_bus.setup_trigger().await?;
    /// ```
    ///
    /// This method is idempotent - it will drop and recreate the trigger if it exists.
    pub async fn setup_trigger(&self) -> Result<(), SqlxError> {
        // Drop existing trigger if present
        sqlx::query(
            r#"
            DROP TRIGGER IF EXISTS epoch_event_bus_notify_trigger ON epoch_events;
            "#,
        )
        .execute(&self.pool)
        .await?;

        // Create the trigger that calls the function after an INSERT.
        // We escape single quotes in the channel name to prevent SQL injection.
        let create_trigger_query = format!(
            r#"
            CREATE TRIGGER epoch_event_bus_notify_trigger
            AFTER INSERT ON epoch_events
            FOR EACH ROW
            EXECUTE FUNCTION epoch_notify_event('{}');
            "#,
            self.channel_name.replace('\'', "''")
        );

        sqlx::query(&create_trigger_query)
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    /// Starts the background listener for event notifications.
    ///
    /// This spawns a tokio task that listens for PostgreSQL NOTIFY messages
    /// and dispatches events to registered projections.
    ///
    /// # Usage
    /// Call this after running migrations and setting up the trigger:
    /// ```rust,ignore
    /// // Run migrations first
    /// Migrator::new(pool.clone()).run().await?;
    /// // Set up the channel-specific trigger
    /// event_bus.setup_trigger().await?;
    /// // Start the background listener
    /// event_bus.start_listener().await?;
    /// ```
    pub async fn start_listener(&self) -> Result<(), SqlxError> {
        let listener_pool = self.pool.clone();
        let checkpoint_pool = self.pool.clone();
        let dlq_pool = self.pool.clone();
        let channel_name = self.channel_name.clone();
        let projections = self.projections.clone();
        let config = self.config.clone();

        tokio::spawn(async move {
            let mut listener_option: Option<PgListener> = None;
            let mut reconnect_delay = Duration::from_secs(1);
            const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(60);

            // In-memory checkpoint cache: subscriber_id -> last_global_sequence
            // This avoids DB round-trips on every event for checkpoint lookups.
            // Cache is populated on first event for each subscriber and updated after DB writes.
            let mut checkpoint_cache: HashMap<String, u64> = HashMap::new();

            // Pending checkpoints for batched mode: subscriber_id -> PendingCheckpoint
            // These are checkpoints that haven't been flushed to the database yet.
            let mut pending_checkpoints: HashMap<String, PendingCheckpoint> = HashMap::new();

            // Interval for periodic checkpoint flush (only used in Batched mode)
            let flush_interval = Duration::from_secs(1);

            loop {
                // Ensure listener is connected
                let listener = match listener_option {
                    Some(ref mut l) => l,
                    None => {
                        // Before reconnecting, flush any pending checkpoints
                        if !pending_checkpoints.is_empty() {
                            info!(
                                "Flushing {} pending checkpoints before reconnection",
                                pending_checkpoints.len()
                            );
                            flush_all_pending_checkpoints(
                                &checkpoint_pool,
                                &mut pending_checkpoints,
                                &mut checkpoint_cache,
                            )
                            .await;
                        }

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

                // Use select! to handle both notification reception and periodic checkpoint flush
                let notification_result = tokio::select! {
                    result = listener.recv() => Some(result),
                    _ = sleep(flush_interval) => {
                        // Periodic flush of expired checkpoints (for Batched mode)
                        flush_expired_checkpoints(
                            &checkpoint_pool,
                            &mut pending_checkpoints,
                            &mut checkpoint_cache,
                            &config.checkpoint_mode,
                        )
                        .await;
                        None // No notification, continue loop
                    }
                };

                let Some(result) = notification_result else {
                    continue; // Was a timer tick, go back to select!
                };

                match result {
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
                            global_sequence: db_event.global_sequence.map(|gs| gs as u64),
                        };

                        // Wrap event in Arc once for efficient sharing across projections
                        let event = Arc::new(event);
                        let event_global_seq = event.global_sequence.unwrap_or(0);

                        let mut projections_guard = projections.lock().await;
                        for projection in projections_guard.iter_mut() {
                            let projection_guard = projection.lock().await;
                            let subscriber_id = projection_guard.subscriber_id().to_string();

                            // Check cached checkpoint first, fall back to DB on cache miss
                            let cached_checkpoint = checkpoint_cache.get(&subscriber_id).copied();
                            let checkpoint_seq = match cached_checkpoint {
                                Some(seq) => Some(seq),
                                None => {
                                    // Cache miss - load from DB and populate cache
                                    match sqlx::query_as::<_, (i64,)>(
                                        r#"
                                        SELECT last_global_sequence
                                        FROM epoch_event_bus_checkpoints
                                        WHERE subscriber_id = $1
                                        "#,
                                    )
                                    .bind(&subscriber_id)
                                    .fetch_optional(&checkpoint_pool)
                                    .await
                                    {
                                        Ok(Some((seq,))) => {
                                            let seq_u64 = seq as u64;
                                            checkpoint_cache.insert(subscriber_id.clone(), seq_u64);
                                            Some(seq_u64)
                                        }
                                        Ok(None) => {
                                            // No checkpoint exists yet - cache 0 to avoid repeated DB lookups
                                            checkpoint_cache.insert(subscriber_id.clone(), 0);
                                            None
                                        }
                                        Err(e) => {
                                            warn!(
                                                "Failed to read checkpoint for '{}': {}, processing event anyway",
                                                subscriber_id, e
                                            );
                                            None
                                        }
                                    }
                                }
                            };

                            if let Some(last_seq) = checkpoint_seq
                                && event_global_seq <= last_seq
                            {
                                log::debug!(
                                    "Skipping duplicate event {} for '{}' (seq {} <= checkpoint {})",
                                    event.id,
                                    subscriber_id,
                                    event_global_seq,
                                    last_seq
                                );
                                continue;
                            }

                            log::debug!(
                                "Applying event to projection '{}': {:?}",
                                subscriber_id,
                                event.id
                            );

                            // Release the lock before retry loop to avoid holding it during retries
                            drop(projection_guard);

                            // Retry loop with exponential backoff
                            let mut last_error: Option<String> = None;
                            let mut succeeded = false;

                            for attempt in 0..=config.max_retries {
                                let projection_guard = projection.lock().await;
                                match projection_guard.on_event(Arc::clone(&event)).await {
                                    Ok(_) => {
                                        log::debug!(
                                            "Successfully applied event to projection '{}': {:?}{}",
                                            subscriber_id,
                                            event.id,
                                            if attempt > 0 {
                                                format!(" (after {} retries)", attempt)
                                            } else {
                                                String::new()
                                            }
                                        );
                                        succeeded = true;
                                        break;
                                    }
                                    Err(e) => {
                                        last_error = Some(format!("{:?}", e));
                                        if attempt < config.max_retries {
                                            let delay = calculate_retry_delay(&config, attempt);
                                            warn!(
                                                "Failed applying event {} to projection '{}' (attempt {}/{}): {:?}. Retrying in {:?}",
                                                event.id,
                                                subscriber_id,
                                                attempt + 1,
                                                config.max_retries + 1,
                                                e,
                                                delay
                                            );
                                            drop(projection_guard);
                                            sleep(delay).await;
                                        } else {
                                            error!(
                                                "Failed applying event {} to projection '{}' after {} attempts: {:?}. Sending to DLQ.",
                                                event.id,
                                                subscriber_id,
                                                config.max_retries + 1,
                                                e
                                            );
                                        }
                                    }
                                }
                            }

                            if !succeeded {
                                // Insert into DLQ after all retries exhausted
                                if let Err(e) = sqlx::query(
                                    r#"
                                    INSERT INTO epoch_event_bus_dlq (subscriber_id, event_id, global_sequence, error_message, retry_count, last_retry_at)
                                    VALUES ($1, $2, $3, $4, $5, NOW())
                                    ON CONFLICT (subscriber_id, event_id) DO UPDATE SET
                                        error_message = EXCLUDED.error_message,
                                        retry_count = EXCLUDED.retry_count,
                                        last_retry_at = NOW()
                                    "#,
                                )
                                .bind(&subscriber_id)
                                .bind(event.id)
                                .bind(event_global_seq as i64)
                                .bind(last_error.as_deref().unwrap_or("Unknown error"))
                                .bind((config.max_retries + 1) as i32)
                                .execute(&dlq_pool)
                                .await
                                {
                                    error!(
                                        "Failed to insert event {} into DLQ for '{}': {}",
                                        event.id, subscriber_id, e
                                    );
                                } else {
                                    info!(
                                        "Event {} for '{}' inserted into DLQ after {} failed attempts",
                                        event.id,
                                        subscriber_id,
                                        config.max_retries + 1
                                    );
                                }
                            }

                            // Update checkpoint (both success and DLQ cases need checkpoint advancement)
                            // Always update in-memory cache immediately for deduplication
                            checkpoint_cache.insert(subscriber_id.clone(), event_global_seq);

                            // Track pending checkpoint for batched mode
                            match pending_checkpoints.get_mut(&subscriber_id) {
                                Some(pending) => {
                                    pending.update(event_global_seq, event.id);
                                }
                                None => {
                                    pending_checkpoints.insert(
                                        subscriber_id.clone(),
                                        PendingCheckpoint::new(event_global_seq, event.id),
                                    );
                                }
                            }

                            // Check if we should flush the checkpoint
                            if let Some(pending) = pending_checkpoints.get(&subscriber_id)
                                && should_flush_checkpoint(pending, &config.checkpoint_mode)
                            {
                                let pending = pending_checkpoints.remove(&subscriber_id).unwrap();
                                flush_checkpoint(
                                    &checkpoint_pool,
                                    &subscriber_id,
                                    &pending,
                                    &mut checkpoint_cache,
                                )
                                .await;
                            }
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

    /// Gets the last checkpoint for a subscriber.
    ///
    /// Returns `None` if no checkpoint exists for the subscriber.
    pub async fn get_checkpoint(&self, subscriber_id: &str) -> Result<Option<u64>, SqlxError> {
        let result: Option<(i64,)> = sqlx::query_as(
            r#"
            SELECT last_global_sequence
            FROM epoch_event_bus_checkpoints
            WHERE subscriber_id = $1
            "#,
        )
        .bind(subscriber_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(result.map(|(seq,)| seq as u64))
    }

    /// Updates or creates a checkpoint for a subscriber.
    ///
    /// This is an upsert operation - it will create a new checkpoint if one doesn't exist,
    /// or update the existing one.
    pub async fn update_checkpoint(
        &self,
        subscriber_id: &str,
        global_sequence: u64,
        event_id: Uuid,
    ) -> Result<(), SqlxError> {
        sqlx::query(
            r#"
            INSERT INTO epoch_event_bus_checkpoints (subscriber_id, last_global_sequence, last_event_id, updated_at)
            VALUES ($1, $2, $3, NOW())
            ON CONFLICT (subscriber_id) DO UPDATE SET
                last_global_sequence = EXCLUDED.last_global_sequence,
                last_event_id = EXCLUDED.last_event_id,
                updated_at = NOW()
            "#,
        )
        .bind(subscriber_id)
        .bind(global_sequence as i64)
        .bind(event_id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Inserts an event into the dead letter queue after all retries have been exhausted.
    ///
    /// This records the failed event along with error information for later analysis
    /// and potential manual retry.
    pub async fn insert_into_dlq(
        &self,
        subscriber_id: &str,
        event_id: Uuid,
        global_sequence: u64,
        error_message: &str,
        retry_count: u32,
    ) -> Result<(), SqlxError> {
        sqlx::query(
            r#"
            INSERT INTO epoch_event_bus_dlq (subscriber_id, event_id, global_sequence, error_message, retry_count, last_retry_at)
            VALUES ($1, $2, $3, $4, $5, NOW())
            ON CONFLICT (subscriber_id, event_id) DO UPDATE SET
                error_message = EXCLUDED.error_message,
                retry_count = EXCLUDED.retry_count,
                last_retry_at = NOW()
            "#,
        )
        .bind(subscriber_id)
        .bind(event_id)
        .bind(global_sequence as i64)
        .bind(error_message)
        .bind(retry_count as i32)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Retrieves all DLQ entries for a specific subscriber.
    pub async fn get_dlq_entries(&self, subscriber_id: &str) -> Result<Vec<DlqEntry>, SqlxError> {
        let rows = sqlx::query(
            r#"
            SELECT id, subscriber_id, event_id, global_sequence, error_message, retry_count, created_at, last_retry_at
            FROM epoch_event_bus_dlq
            WHERE subscriber_id = $1
            ORDER BY created_at ASC
            "#,
        )
        .bind(subscriber_id)
        .fetch_all(&self.pool)
        .await?;

        use sqlx::Row;
        Ok(rows
            .into_iter()
            .map(|row| DlqEntry {
                id: row.get("id"),
                subscriber_id: row.get("subscriber_id"),
                event_id: row.get("event_id"),
                global_sequence: row.get::<i64, _>("global_sequence") as u64,
                error_message: row.get("error_message"),
                retry_count: row.get("retry_count"),
                created_at: row.get("created_at"),
                last_retry_at: row.get("last_retry_at"),
            })
            .collect())
    }

    /// Attempts to acquire an advisory lock for a subscriber.
    ///
    /// This is used for multi-instance coordination in `InstanceMode::Coordinated`.
    /// Uses MD5-based dual-int4 approach for 64-bit key space.
    ///
    /// Returns `true` if the lock was acquired, `false` otherwise.
    pub async fn try_acquire_subscriber_lock(
        &self,
        subscriber_id: &str,
    ) -> Result<bool, SqlxError> {
        let result: (bool,) = sqlx::query_as(
            r#"
            SELECT pg_try_advisory_lock(
                ('x' || substr(md5($1), 1, 8))::bit(32)::int,
                ('x' || substr(md5($1), 9, 8))::bit(32)::int
            )
            "#,
        )
        .bind(subscriber_id)
        .fetch_one(&self.pool)
        .await?;

        Ok(result.0)
    }

    /// Releases an advisory lock for a subscriber.
    ///
    /// Returns `true` if the lock was released, `false` if it wasn't held.
    pub async fn release_subscriber_lock(&self, subscriber_id: &str) -> Result<bool, SqlxError> {
        let result: (bool,) = sqlx::query_as(
            r#"
            SELECT pg_advisory_unlock(
                ('x' || substr(md5($1), 1, 8))::bit(32)::int,
                ('x' || substr(md5($1), 9, 8))::bit(32)::int
            )
            "#,
        )
        .bind(subscriber_id)
        .fetch_one(&self.pool)
        .await?;

        Ok(result.0)
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
        let pool = self.pool.clone();
        let config = self.config.clone();
        let channel_name = self.channel_name.clone();

        Box::pin(async move {
            // Wrap the projector in Arc<Mutex<>> for sharing
            let observer: Arc<Mutex<dyn EventObserver<Self::EventType>>> =
                Arc::new(Mutex::new(projector));

            // Get subscriber_id for catch-up
            let subscriber_id = {
                let guard = observer.lock().await;
                guard.subscriber_id().to_string()
            };

            // === Multi-instance coordination ===
            // If in Coordinated mode, try to acquire an advisory lock.
            // If the lock is already held by another instance, skip this subscription.
            if config.instance_mode == InstanceMode::Coordinated {
                let lock_acquired: (bool,) = sqlx::query_as(
                    r#"
                    SELECT pg_try_advisory_lock(
                        ('x' || substr(md5($1), 1, 8))::bit(32)::int,
                        ('x' || substr(md5($1), 9, 8))::bit(32)::int
                    )
                    "#,
                )
                .bind(&subscriber_id)
                .fetch_one(&pool)
                .await?;

                if !lock_acquired.0 {
                    info!(
                        "Another instance is processing subscriber '{}'. Skipping subscription on this instance.",
                        subscriber_id
                    );
                    return Ok(());
                }

                info!(
                    "Acquired advisory lock for subscriber '{}'. This instance will process events.",
                    subscriber_id
                );
            }

            // === Gap-free catch-up with event buffering ===
            // To prevent race conditions between catch-up and real-time events:
            // 1. Start NOTIFY listener first and buffer incoming events
            // 2. Query events since last checkpoint
            // 3. Process catch-up events with retry/DLQ
            // 4. Drain buffer with deduplication (skip if global_seq <= checkpoint)
            // 5. Add to live projections

            // Create a buffer for events arriving during catch-up
            let event_buffer: Arc<Mutex<Vec<Event<Self::EventType>>>> =
                Arc::new(Mutex::new(Vec::new()));

            // Start a temporary listener to buffer events during catch-up
            let buffer_listener_pool = pool.clone();
            let buffer_channel = channel_name.clone();
            let buffer_ref = event_buffer.clone();

            // Spawn a task to listen and buffer events
            let buffer_handle = tokio::spawn(async move {
                let mut listener = match PgListener::connect_with(&buffer_listener_pool).await {
                    Ok(l) => l,
                    Err(e) => {
                        warn!(
                            "Failed to create buffer listener: {}. Proceeding without buffering.",
                            e
                        );
                        return;
                    }
                };

                if let Err(e) = listener.listen(&buffer_channel).await {
                    warn!(
                        "Failed to listen for buffer: {}. Proceeding without buffering.",
                        e
                    );
                    return;
                }

                log::debug!("Buffer listener started for catch-up");

                // Listen until the channel is closed (when catch-up completes)
                loop {
                    match tokio::time::timeout(Duration::from_millis(100), listener.recv()).await {
                        Ok(Ok(notification)) => {
                            let payload = notification.payload();
                            if let Ok(db_event) = serde_json::from_str::<PgDBEvent>(payload)
                                && let Ok(data) = db_event
                                    .data
                                    .map(|d| serde_json::from_value::<Self::EventType>(d))
                                    .transpose()
                            {
                                let event = Event {
                                    id: db_event.id,
                                    stream_id: db_event.stream_id,
                                    stream_version: db_event.stream_version as u64,
                                    event_type: db_event.event_type,
                                    actor_id: db_event.actor_id,
                                    purger_id: db_event.purger_id,
                                    data,
                                    created_at: db_event.created_at,
                                    purged_at: db_event.purged_at,
                                    global_sequence: db_event.global_sequence.map(|gs| gs as u64),
                                };
                                let mut buffer = buffer_ref.lock().await;
                                buffer.push(event);
                                log::debug!("Buffered event during catch-up: {:?}", db_event.id);
                            }
                        }
                        Ok(Err(_)) => {
                            // Listener error, stop buffering
                            break;
                        }
                        Err(_) => {
                            // Timeout - check if we should continue
                            // The buffer_ref being dropped will signal completion
                            if Arc::strong_count(&buffer_ref) <= 1 {
                                break;
                            }
                        }
                    }
                }
            });

            // Get the last checkpoint for this subscriber
            let last_sequence = {
                let result: Option<(i64,)> = sqlx::query_as(
                    r#"
                    SELECT last_global_sequence
                    FROM epoch_event_bus_checkpoints
                    WHERE subscriber_id = $1
                    "#,
                )
                .bind(&subscriber_id)
                .fetch_optional(&pool)
                .await?;

                result.map(|(seq,)| seq as u64).unwrap_or(0)
            };

            let mut current_sequence = last_sequence;
            let mut total_caught_up = 0u64;

            // Pending checkpoint for batched mode during catch-up
            let mut pending_checkpoint: Option<PendingCheckpoint> = None;
            // Local checkpoint cache for flush_checkpoint
            let mut checkpoint_cache: HashMap<String, u64> = HashMap::new();

            // Process catch-up events from the database
            loop {
                let rows: Vec<PgDBEvent> = sqlx::query_as(
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
                    WHERE global_sequence > $1
                    ORDER BY global_sequence ASC
                    LIMIT $2
                    "#,
                )
                .bind(current_sequence as i64)
                .bind(config.catch_up_batch_size as i64)
                .fetch_all(&pool)
                .await?;

                if rows.is_empty() {
                    break;
                }

                let batch_size = rows.len();
                if total_caught_up == 0 && !rows.is_empty() {
                    info!(
                        "Catch-up for '{}': starting from sequence {}, found events to process",
                        subscriber_id, current_sequence
                    );
                }

                for row in rows {
                    let data: Option<Self::EventType> =
                        match row.data.map(|d| serde_json::from_value(d)).transpose() {
                            Ok(d) => d,
                            Err(e) => {
                                error!(
                                    "Catch-up: failed to deserialize event {} for '{}': {}",
                                    row.id, subscriber_id, e
                                );
                                continue;
                            }
                        };

                    let event_global_seq = row.global_sequence.unwrap_or(0) as u64;
                    let event_id = row.id;

                    let event = Arc::new(Event {
                        id: row.id,
                        stream_id: row.stream_id,
                        stream_version: row.stream_version as u64,
                        event_type: row.event_type,
                        actor_id: row.actor_id,
                        purger_id: row.purger_id,
                        data,
                        created_at: row.created_at,
                        purged_at: row.purged_at,
                        global_sequence: Some(event_global_seq),
                    });

                    // Use the same retry/DLQ logic as real-time processing
                    let result =
                        process_event_with_retry(&observer, &event, &subscriber_id, &config, &pool)
                            .await;

                    // Track pending checkpoint for batched mode
                    match &mut pending_checkpoint {
                        Some(pending) => {
                            pending.update(event_global_seq, event_id);
                        }
                        None => {
                            pending_checkpoint =
                                Some(PendingCheckpoint::new(event_global_seq, event_id));
                        }
                    }

                    // Check if we should flush the checkpoint
                    if let Some(ref pending) = pending_checkpoint
                        && should_flush_checkpoint(pending, &config.checkpoint_mode)
                    {
                        let pending = pending_checkpoint.take().unwrap();
                        flush_checkpoint(&pool, &subscriber_id, &pending, &mut checkpoint_cache)
                            .await;
                    }

                    current_sequence = event_global_seq;
                    total_caught_up += 1;

                    if let ProcessResult::Success = result {
                        log::debug!(
                            "Catch-up: processed event {} for '{}'",
                            event_id,
                            subscriber_id
                        );
                    }
                }

                // If we got fewer events than the batch size, we've caught up
                if batch_size < config.catch_up_batch_size as usize {
                    break;
                }
            }

            // Flush any remaining pending checkpoint after catch-up
            if let Some(pending) = pending_checkpoint.take() {
                flush_checkpoint(&pool, &subscriber_id, &pending, &mut checkpoint_cache).await;
            }

            if total_caught_up > 0 {
                info!(
                    "Catch-up complete for '{}': processed {} events, checkpoint now at {}",
                    subscriber_id, total_caught_up, current_sequence
                );
            }

            // Stop the buffer listener
            buffer_handle.abort();

            // Drain and process buffered events with deduplication
            let buffered_events = {
                let mut buffer = event_buffer.lock().await;
                std::mem::take(&mut *buffer)
            };

            let buffered_count = buffered_events.len();
            let mut processed_from_buffer = 0u64;

            for event in buffered_events {
                let event_global_seq = event.global_sequence.unwrap_or(0);

                // Deduplicate: skip events we've already processed during catch-up
                if event_global_seq <= current_sequence {
                    log::debug!(
                        "Skipping buffered event {} (seq {} <= checkpoint {})",
                        event.id,
                        event_global_seq,
                        current_sequence
                    );
                    continue;
                }

                let event_id = event.id;
                let event = Arc::new(event);

                let result =
                    process_event_with_retry(&observer, &event, &subscriber_id, &config, &pool)
                        .await;

                // Track pending checkpoint for batched mode
                match &mut pending_checkpoint {
                    Some(pending) => {
                        pending.update(event_global_seq, event_id);
                    }
                    None => {
                        pending_checkpoint =
                            Some(PendingCheckpoint::new(event_global_seq, event_id));
                    }
                }

                // Check if we should flush the checkpoint
                if let Some(ref pending) = pending_checkpoint
                    && should_flush_checkpoint(pending, &config.checkpoint_mode)
                {
                    let pending = pending_checkpoint.take().unwrap();
                    flush_checkpoint(&pool, &subscriber_id, &pending, &mut checkpoint_cache).await;
                }

                current_sequence = event_global_seq;
                processed_from_buffer += 1;

                if let ProcessResult::Success = result {
                    log::debug!(
                        "Processed buffered event {} for '{}'",
                        event_id,
                        subscriber_id
                    );
                }
            }

            // Flush any remaining pending checkpoint after buffer processing
            if let Some(pending) = pending_checkpoint.take() {
                flush_checkpoint(&pool, &subscriber_id, &pending, &mut checkpoint_cache).await;
            }

            if buffered_count > 0 {
                info!(
                    "Processed {} buffered events for '{}' ({} deduplicated), checkpoint now at {}",
                    processed_from_buffer,
                    subscriber_id,
                    buffered_count as u64 - processed_from_buffer,
                    current_sequence
                );
            }

            // Now add to live projections for real-time events
            let mut projections = projections.lock().await;
            projections.push(observer);

            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reliable_delivery_config_has_sensible_defaults() {
        let config = ReliableDeliveryConfig::default();

        assert_eq!(config.max_retries, 3);
        assert_eq!(config.initial_retry_delay, Duration::from_secs(1));
        assert_eq!(config.max_retry_delay, Duration::from_secs(60));
        assert_eq!(config.checkpoint_mode, CheckpointMode::Synchronous);
        assert_eq!(config.instance_mode, InstanceMode::SingleInstance);
        assert_eq!(config.catch_up_batch_size, 100);
    }

    #[test]
    fn checkpoint_mode_default_is_synchronous() {
        assert_eq!(CheckpointMode::default(), CheckpointMode::Synchronous);
    }

    #[test]
    fn instance_mode_default_is_single_instance() {
        assert_eq!(InstanceMode::default(), InstanceMode::SingleInstance);
    }

    #[test]
    fn instance_mode_coordinated_exists_and_differs_from_single() {
        let coordinated = InstanceMode::Coordinated;
        let single = InstanceMode::SingleInstance;
        assert_ne!(coordinated, single);
    }

    #[test]
    fn reliable_delivery_config_can_be_customized() {
        let config = ReliableDeliveryConfig {
            max_retries: 5,
            initial_retry_delay: Duration::from_millis(500),
            max_retry_delay: Duration::from_secs(120),
            checkpoint_mode: CheckpointMode::Synchronous,
            instance_mode: InstanceMode::SingleInstance,
            catch_up_batch_size: 500,
        };

        assert_eq!(config.max_retries, 5);
        assert_eq!(config.initial_retry_delay, Duration::from_millis(500));
        assert_eq!(config.max_retry_delay, Duration::from_secs(120));
        assert_eq!(config.checkpoint_mode, CheckpointMode::Synchronous);
        assert_eq!(config.instance_mode, InstanceMode::SingleInstance);
        assert_eq!(config.catch_up_batch_size, 500);
    }

    #[test]
    fn retry_delay_calculation_exponential_backoff() {
        let config = ReliableDeliveryConfig {
            initial_retry_delay: Duration::from_secs(1),
            max_retry_delay: Duration::from_secs(60),
            ..Default::default()
        };

        // First attempt: 1 second
        assert_eq!(calculate_retry_delay(&config, 0), Duration::from_secs(1));
        // Second attempt: 2 seconds
        assert_eq!(calculate_retry_delay(&config, 1), Duration::from_secs(2));
        // Third attempt: 4 seconds
        assert_eq!(calculate_retry_delay(&config, 2), Duration::from_secs(4));
        // Fourth attempt: 8 seconds
        assert_eq!(calculate_retry_delay(&config, 3), Duration::from_secs(8));
        // Fifth attempt: 16 seconds
        assert_eq!(calculate_retry_delay(&config, 4), Duration::from_secs(16));
        // Sixth attempt: 32 seconds
        assert_eq!(calculate_retry_delay(&config, 5), Duration::from_secs(32));
        // Seventh attempt: would be 64, but capped at 60
        assert_eq!(calculate_retry_delay(&config, 6), Duration::from_secs(60));
        // Eighth attempt: still capped at 60
        assert_eq!(calculate_retry_delay(&config, 7), Duration::from_secs(60));
    }

    #[test]
    fn retry_delay_with_millisecond_precision() {
        let config = ReliableDeliveryConfig {
            initial_retry_delay: Duration::from_millis(100),
            max_retry_delay: Duration::from_millis(1000),
            ..Default::default()
        };

        assert_eq!(
            calculate_retry_delay(&config, 0),
            Duration::from_millis(100)
        );
        assert_eq!(
            calculate_retry_delay(&config, 1),
            Duration::from_millis(200)
        );
        assert_eq!(
            calculate_retry_delay(&config, 2),
            Duration::from_millis(400)
        );
        assert_eq!(
            calculate_retry_delay(&config, 3),
            Duration::from_millis(800)
        );
        // Capped at 1000ms
        assert_eq!(
            calculate_retry_delay(&config, 4),
            Duration::from_millis(1000)
        );
    }

    #[test]
    fn retry_delay_handles_large_attempt_numbers() {
        let config = ReliableDeliveryConfig {
            initial_retry_delay: Duration::from_secs(1),
            max_retry_delay: Duration::from_secs(60),
            ..Default::default()
        };

        // Very large attempt numbers should be capped and not overflow
        assert_eq!(calculate_retry_delay(&config, 100), Duration::from_secs(60));
        assert_eq!(
            calculate_retry_delay(&config, 1000),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn dlq_entry_structure() {
        let entry = DlqEntry {
            id: Uuid::new_v4(),
            subscriber_id: "projection:test".to_string(),
            event_id: Uuid::new_v4(),
            global_sequence: 42,
            error_message: Some("Test error".to_string()),
            retry_count: 3,
            created_at: chrono::Utc::now(),
            last_retry_at: Some(chrono::Utc::now()),
        };

        assert_eq!(entry.subscriber_id, "projection:test");
        assert_eq!(entry.global_sequence, 42);
        assert_eq!(entry.retry_count, 3);
        assert!(entry.error_message.is_some());
    }

    #[test]
    fn checkpoint_mode_batched_exists_and_differs_from_synchronous() {
        let batched = CheckpointMode::Batched {
            batch_size: 100,
            max_delay_ms: 5000,
        };
        let synchronous = CheckpointMode::Synchronous;
        assert_ne!(batched, synchronous);
    }

    #[test]
    fn checkpoint_mode_batched_helper_creates_correct_mode() {
        let mode = CheckpointMode::batched(50, Duration::from_secs(10));
        assert_eq!(
            mode,
            CheckpointMode::Batched {
                batch_size: 50,
                max_delay_ms: 10000,
            }
        );
    }

    #[test]
    fn checkpoint_mode_batched_default_has_expected_values() {
        let mode = CheckpointMode::batched_default();
        assert_eq!(
            mode,
            CheckpointMode::Batched {
                batch_size: 100,
                max_delay_ms: 5000,
            }
        );
    }

    #[test]
    fn pending_checkpoint_new_initializes_correctly() {
        let event_id = Uuid::new_v4();
        let pending = PendingCheckpoint::new(42, event_id);

        assert_eq!(pending.global_sequence, 42);
        assert_eq!(pending.event_id, event_id);
        assert_eq!(pending.events_since_checkpoint, 1);
        // first_event_time should be very recent
        assert!(pending.first_event_time.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn pending_checkpoint_update_increments_count() {
        let event_id1 = Uuid::new_v4();
        let event_id2 = Uuid::new_v4();
        let mut pending = PendingCheckpoint::new(42, event_id1);

        pending.update(43, event_id2);

        assert_eq!(pending.global_sequence, 43);
        assert_eq!(pending.event_id, event_id2);
        assert_eq!(pending.events_since_checkpoint, 2);
    }

    #[test]
    fn should_flush_synchronous_always_returns_true() {
        let pending = PendingCheckpoint::new(1, Uuid::new_v4());
        assert!(should_flush_checkpoint(
            &pending,
            &CheckpointMode::Synchronous
        ));
    }

    #[test]
    fn should_flush_batched_returns_true_at_batch_size() {
        let event_id = Uuid::new_v4();
        let mut pending = PendingCheckpoint::new(1, event_id);

        let mode = CheckpointMode::Batched {
            batch_size: 5,
            max_delay_ms: 60000, // 60 seconds - won't trigger
        };

        // 1 event - should not flush
        assert!(!should_flush_checkpoint(&pending, &mode));

        // 2, 3, 4 events - should not flush
        for i in 2..5 {
            pending.update(i, Uuid::new_v4());
            assert!(!should_flush_checkpoint(&pending, &mode));
        }

        // 5th event - should flush
        pending.update(5, Uuid::new_v4());
        assert!(should_flush_checkpoint(&pending, &mode));
    }

    #[test]
    fn should_flush_batched_returns_true_at_max_delay() {
        // Create a pending checkpoint with an old first_event_time
        let mut pending = PendingCheckpoint::new(1, Uuid::new_v4());
        // Simulate time passing by creating with an artificially old time
        pending.first_event_time = Instant::now() - Duration::from_millis(600);

        let mode = CheckpointMode::Batched {
            batch_size: 100, // Won't reach this
            max_delay_ms: 500,
        };

        // Even with only 1 event, should flush due to time
        assert!(should_flush_checkpoint(&pending, &mode));
    }

    #[test]
    fn should_flush_batched_returns_false_before_thresholds() {
        let pending = PendingCheckpoint::new(1, Uuid::new_v4());

        let mode = CheckpointMode::Batched {
            batch_size: 100,
            max_delay_ms: 60000,
        };

        // Neither batch_size nor max_delay reached
        assert!(!should_flush_checkpoint(&pending, &mode));
    }

    #[test]
    fn reliable_delivery_config_can_use_batched_checkpoint_mode() {
        let config = ReliableDeliveryConfig {
            checkpoint_mode: CheckpointMode::batched(50, Duration::from_secs(3)),
            ..Default::default()
        };

        assert_eq!(
            config.checkpoint_mode,
            CheckpointMode::Batched {
                batch_size: 50,
                max_delay_ms: 3000,
            }
        );
    }
}
