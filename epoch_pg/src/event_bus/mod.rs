//! This module defines the `PgEventBus` that implements epoch_core::EventBus using PostgreSQL's
//! LISTEN/NOTIFY feature.

mod checkpoint;
mod config;
mod retry;
mod subscriber_state;

pub(crate) use checkpoint::*;
pub use config::{CheckpointMode, DlqCallback, DlqInsertionInfo, InstanceMode, ReliableDeliveryConfig};
pub(crate) use retry::{ProcessResult, process_event_with_retry};
pub(crate) use subscriber_state::{SubscriberState, advance_contiguous_checkpoint};

#[cfg(test)]
pub use retry::calculate_retry_delay_no_jitter;

use crate::event_store::PgDBEvent;
use epoch_core::event::{Event, EventData};
use epoch_core::event_store::EventBus;
use epoch_core::prelude::EventObserver;
use log::{error, info, warn};
use serde::de::DeserializeOwned;
use sqlx::Error as SqlxError;
use sqlx::postgres::{PgListener, PgPool};
use std::collections::{BTreeSet, HashMap};
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Duration, sleep};
use uuid::Uuid;

use std::future::Future;

/// Represents an entry in the dead letter queue.
///
/// DLQ entries are created when event processing fails after all retry attempts
/// are exhausted. They can be queried for monitoring and manually resolved
/// after investigation.
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
    /// When the entry was manually resolved (None if unresolved)
    pub resolved_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Identifier of the operator/system that resolved the entry
    pub resolved_by: Option<String>,
    /// Free-form notes about the resolution
    pub resolution_notes: Option<String>,
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

/// Type alias for the projections collection to reduce type complexity.
///
/// # Design Note
///
/// The nested `Arc<Mutex<Vec<Arc<Mutex<...>>>>>` structure is intentional:
///
/// - **Outer `Arc<Mutex<Vec<...>>>`**: Allows thread-safe access to the list of projections.
///   We use `Mutex` instead of `RwLock` because:
///   - Lock duration is very short (just to push/iterate)
///   - `subscribe()` calls during startup are relatively common
///   - `Mutex` has lower overhead than `RwLock` for short critical sections
///   - We don't benefit from concurrent reads since iteration is fast
///
/// - **Inner `Arc<Mutex<dyn EventObserver<D>>>`**: Each projection needs thread-safe
///   interior mutability for the retry loop, which must release the lock between
///   retry attempts to avoid deadlocks.
///
/// # Performance Considerations
///
/// The outer `Mutex` is held while iterating through projections in the listener loop.
/// For high-throughput scenarios with frequent `subscribe()` calls during runtime,
/// consider using a `RwLock` or a lock-free concurrent data structure (like
/// `crossbeam`'s `SkipMap`) to allow concurrent reads during event dispatch while
/// writes (new subscriptions) wait. However, for most use cases where subscriptions
/// happen at startup, the current design is sufficient.
type Projections<D> = Arc<Mutex<Vec<Arc<Mutex<dyn EventObserver<D>>>>>>;

/// Internal state for managing the listener lifecycle.
struct ListenerState {
    /// Handle to the spawned listener task.
    handle: tokio::task::JoinHandle<()>,
    /// Signal to trigger shutdown.
    shutdown_tx: tokio::sync::watch::Sender<bool>,
}

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
    /// Listener lifecycle state, set when `start_listener` is called.
    listener_state: Arc<Mutex<Option<ListenerState>>>,
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
            listener_state: Arc::new(Mutex::new(None)),
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
                global_sequence,
                causation_id,
                correlation_id
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
            let data: Option<D> = match row.data.map(|d| serde_json::from_value(d)).transpose() {
                Ok(d) => d,
                Err(e) => {
                    warn!(
                        "Skipping event {} (type: '{}', global_seq: {:?}): failed to deserialize: {}. \
                         This is expected when event variants have been removed from the application enum.",
                        row.id, row.event_type, row.global_sequence, e
                    );
                    continue;
                }
            };

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
                causation_id: row.causation_id,
                correlation_id: row.correlation_id,
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
    ///
    /// # Lifecycle Management
    ///
    /// This method spawns a background task that runs indefinitely. The task handles
    /// reconnection automatically on connection failures.
    ///
    /// For graceful shutdown, use the [`shutdown`](Self::shutdown) method which will:
    /// 1. Signal the listener to stop accepting new events
    /// 2. Flush any pending checkpoints (important for `Batched` mode)
    /// 3. Wait for the listener task to complete
    ///
    /// ```rust,ignore
    /// // Graceful shutdown
    /// event_bus.shutdown().await?;
    /// ```
    pub async fn start_listener(&self) -> Result<(), SqlxError> {
        // Check if already started
        {
            let state = self.listener_state.lock().await;
            if state.is_some() {
                warn!("Listener already started, ignoring duplicate start_listener call");
                return Ok(());
            }
        }

        let listener_pool = self.pool.clone();
        let checkpoint_pool = self.pool.clone();
        let dlq_pool = self.pool.clone();
        let channel_name = self.channel_name.clone();
        let projections = self.projections.clone();
        let config = self.config.clone();

        // Create shutdown signal channel
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

        let handle = tokio::spawn(async move {
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

            // Per-subscriber gap-aware state: tracks contiguous checkpoint and out-of-order
            // processed events. Replaces the simple checkpoint_cache for deduplication.
            let mut subscriber_states: HashMap<String, SubscriberState> = HashMap::new();

            // Tracks the event UUID associated with the most recently flushed contiguous
            // checkpoint per subscriber. Used when updating pending_checkpoints after
            // advance_contiguous_checkpoint (including gap-timeout advances with no event).
            let mut last_event_ids: HashMap<String, Uuid> = HashMap::new();

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

                // Tracks whether the select! woke us via a notification or a timer tick.
                // In both cases we want to process pending events; the timer tick also
                // flushes expired checkpoints for Batched mode.
                enum WakeReason {
                    Notification(Result<sqlx::postgres::PgNotification, sqlx::Error>),
                    TimerTick,
                    Shutdown,
                }

                let wake_reason = tokio::select! {
                    result = listener.recv() => WakeReason::Notification(result),
                    _ = sleep(flush_interval) => {
                        // Periodic flush of expired checkpoints (for Batched mode)
                        flush_expired_checkpoints(
                            &checkpoint_pool,
                            &mut pending_checkpoints,
                            &mut checkpoint_cache,
                            &config.checkpoint_mode,
                        )
                        .await;
                        WakeReason::TimerTick
                    }
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            info!("Shutdown signal received, flushing pending checkpoints...");
                            flush_all_pending_checkpoints(
                                &checkpoint_pool,
                                &mut pending_checkpoints,
                                &mut checkpoint_cache,
                            )
                            .await;
                            info!("Listener shutdown complete");
                            return;
                        }
                        WakeReason::Shutdown
                    }
                };

                // Handle notification errors and reconnection
                match wake_reason {
                    WakeReason::Notification(Err(e)) => {
                        error!(
                            "Error receiving notification: {}. Attempting to reconnect...",
                            e
                        );
                        listener_option = None;
                        sleep(reconnect_delay).await;
                        reconnect_delay = (reconnect_delay * 2).min(MAX_RECONNECT_DELAY);
                        continue;
                    }
                    WakeReason::Notification(Ok(_notification)) => {
                        reconnect_delay = Duration::from_secs(1);
                        log::debug!("NOTIFY received, querying DB for new committed events");
                    }
                    WakeReason::TimerTick => {
                        log::trace!(
                            "Periodic tick, checking for pending events and gap resolution"
                        );
                    }
                    WakeReason::Shutdown => {
                        continue;
                    }
                }

                // === Event processing for ALL subscribers ===
                // This block runs for both NOTIFY and TimerTick wake reasons.
                // For NOTIFY: processes newly committed events.
                // For TimerTick: fills gaps from delayed commits, advances past
                //   timed-out gaps (rolled-back transactions), catches missed NOTIFYs.

                // NOTE: The projections lock is held while iterating and processing events.
                // This means new subscribers cannot be added during event processing.
                let mut projections_guard = projections.lock().await;
                for projection in projections_guard.iter_mut() {
                    let subscriber_id = {
                        let guard = projection.lock().await;
                        guard.subscriber_id().to_string()
                    };

                    // Initialize per-subscriber state on first encounter.
                    // Load the persisted checkpoint from the DB as the starting point.
                    if !subscriber_states.contains_key(&subscriber_id) {
                        let checkpoint = match sqlx::query_as::<_, (i64,)>(
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
                            Ok(Some((seq,))) => seq as u64,
                            Ok(None) => 0,
                            Err(e) => {
                                warn!(
                                    "Failed to load checkpoint for '{}': {}, starting from 0",
                                    subscriber_id, e
                                );
                                0
                            }
                        };
                        subscriber_states
                            .insert(subscriber_id.clone(), SubscriberState::new(checkpoint));
                    }

                    // Process all pending events in batches until caught up.
                    // Each NOTIFY may represent multiple committed events if they arrived
                    // in rapid succession, so we loop until the DB returns fewer rows
                    // than the batch size.
                    loop {
                        let contiguous_before = subscriber_states
                            .get(&subscriber_id)
                            .unwrap()
                            .contiguous_checkpoint;

                        let rows: Vec<PgDBEvent> = match sqlx::query_as(
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
                                        global_sequence,
                                        causation_id,
                                        correlation_id
                                    FROM epoch_events
                                    WHERE global_sequence > $1
                                    ORDER BY global_sequence ASC
                                    LIMIT $2
                                    "#,
                        )
                        .bind(contiguous_before as i64)
                        .bind(config.catch_up_batch_size as i64)
                        .fetch_all(&checkpoint_pool)
                        .await
                        {
                            Ok(rows) => rows,
                            Err(e) => {
                                error!(
                                    "Failed to query events for subscriber '{}': {}",
                                    subscriber_id, e
                                );
                                break;
                            }
                        };

                        if rows.is_empty() {
                            break;
                        }

                        let batch_was_full = rows.len() == config.catch_up_batch_size as usize;

                        // Collect visible sequence numbers for gap detection in
                        // advance_contiguous_checkpoint.
                        let visible_seqs: BTreeSet<u64> = rows
                            .iter()
                            .filter_map(|r| r.global_sequence.map(|gs| gs as u64))
                            .collect();

                        // Map seq → event_id so we can associate the right event_id
                        // with the contiguous checkpoint after it advances.
                        let seq_to_id: HashMap<u64, Uuid> = rows
                            .iter()
                            .filter_map(|r| r.global_sequence.map(|gs| (gs as u64, r.id)))
                            .collect();

                        // Track whether any new events were processed in this
                        // iteration. If all events were already in processed_ahead
                        // (e.g., a gap is blocking checkpoint advancement but the
                        // batch is full), we break to avoid an infinite hot loop
                        // hammering the DB while waiting for gap_timeout to expire.
                        let mut processed_any = false;
                        for row in rows {
                            let event_seq = row.global_sequence.unwrap_or(0) as u64;
                            let event_id = row.id;

                            // Skip events already processed out-of-order.
                            // Use a short-lived borrow so it's dropped before the await.
                            let already_processed = {
                                let state = subscriber_states.get(&subscriber_id).unwrap();
                                state.processed_ahead.contains(&event_seq)
                            };
                            if already_processed {
                                last_event_ids.insert(subscriber_id.clone(), event_id);
                                continue;
                            }

                            // Deserialize event data. Undeserializable events (e.g.,
                            // removed enum variants) are marked processed and skipped
                            // so the checkpoint can advance past them.
                            let data = match row
                                .data
                                .map(|d| serde_json::from_value::<D>(d))
                                .transpose()
                            {
                                Ok(data) => data,
                                Err(e) => {
                                    warn!(
                                        "Skipping event {} (type: '{}', global_seq: {}) \
                                                 for '{}': failed to deserialize: {}. \
                                                 This is expected when event variants have been \
                                                 removed. Advancing checkpoint past this event.",
                                        event_id, row.event_type, event_seq, subscriber_id, e
                                    );
                                    let state = subscriber_states.get_mut(&subscriber_id).unwrap();
                                    state.processed_ahead.insert(event_seq);
                                    last_event_ids.insert(subscriber_id.clone(), event_id);
                                    // Count skipped event toward batched checkpoint
                                    // threshold so batch_size triggers correctly.
                                    match pending_checkpoints.get_mut(&subscriber_id) {
                                        Some(pending) => {
                                            pending.update(event_seq, event_id);
                                        }
                                        None => {
                                            pending_checkpoints.insert(
                                                subscriber_id.clone(),
                                                PendingCheckpoint::new(event_seq, event_id),
                                            );
                                        }
                                    }
                                    processed_any = true;
                                    continue;
                                }
                            };

                            let event = Arc::new(Event::<D> {
                                id: event_id,
                                stream_id: row.stream_id,
                                stream_version: row.stream_version as u64,
                                event_type: row.event_type,
                                actor_id: row.actor_id,
                                purger_id: row.purger_id,
                                data,
                                created_at: row.created_at,
                                purged_at: row.purged_at,
                                global_sequence: Some(event_seq),
                                causation_id: row.causation_id,
                                correlation_id: row.correlation_id,
                            });

                            log::debug!(
                                "Applying event {} (seq {}) to projection '{}'",
                                event_id,
                                event_seq,
                                subscriber_id
                            );

                            // Process with retry/DLQ. No state borrow held across await.
                            process_event_with_retry(
                                projection,
                                &event,
                                &subscriber_id,
                                &config,
                                &dlq_pool,
                            )
                            .await;

                            // Mark as processed so advance_contiguous_checkpoint can
                            // use it to advance through this sequence.
                            {
                                let state = subscriber_states.get_mut(&subscriber_id).unwrap();
                                state.processed_ahead.insert(event_seq);
                            }
                            last_event_ids.insert(subscriber_id.clone(), event_id);
                            // Count each newly-processed event toward the batched
                            // checkpoint threshold so batch_size triggers correctly.
                            // global_sequence is set to a safe contiguous position
                            // below after advance_contiguous_checkpoint runs.
                            match pending_checkpoints.get_mut(&subscriber_id) {
                                Some(pending) => {
                                    pending.update(event_seq, event_id);
                                }
                                None => {
                                    pending_checkpoints.insert(
                                        subscriber_id.clone(),
                                        PendingCheckpoint::new(event_seq, event_id),
                                    );
                                }
                            }
                            processed_any = true;
                        }

                        // Advance the contiguous checkpoint as far as possible.
                        // This handles gaps from out-of-order commits and rolled-back
                        // transactions (via the gap_timeout).
                        {
                            let state = subscriber_states.get_mut(&subscriber_id).unwrap();
                            advance_contiguous_checkpoint(state, &visible_seqs, config.gap_timeout);
                        }

                        let new_contiguous = subscriber_states
                            .get(&subscriber_id)
                            .unwrap()
                            .contiguous_checkpoint;

                        // Only flush the pending checkpoint when it actually advanced to
                        // a contiguous position. If there's a gap blocking advancement,
                        // we must not persist a checkpoint that would skip unprocessed
                        // events. Note: events_since_checkpoint is tracked per-event
                        // in the for loop above for accurate batch_size triggering.
                        if new_contiguous > contiguous_before {
                            // Associate the event_id at the new checkpoint position.
                            // Falls back to the last known event_id when the checkpoint
                            // advanced past a timed-out gap (no corresponding event).
                            let checkpoint_event_id = seq_to_id
                                .get(&new_contiguous)
                                .copied()
                                .or_else(|| last_event_ids.get(&subscriber_id).copied())
                                .unwrap_or_else(Uuid::nil);

                            last_event_ids.insert(subscriber_id.clone(), checkpoint_event_id);

                            // Keep checkpoint_cache in sync with contiguous_checkpoint.
                            // This mirrors the authoritative subscriber_states value so
                            // future reads of checkpoint_cache are never stale.
                            checkpoint_cache.insert(subscriber_id.clone(), new_contiguous);

                            // Override global_sequence and event_id to the safe
                            // contiguous position. events_since_checkpoint was already
                            // incremented per-event in the for loop, so we must NOT
                            // call update() here (that would double-count).
                            if let Some(pending) = pending_checkpoints.get_mut(&subscriber_id) {
                                pending.global_sequence = new_contiguous;
                                pending.event_id = checkpoint_event_id;
                            } else {
                                // No events were processed in this iteration (all
                                // already in processed_ahead, e.g., gap timed out and
                                // checkpoint advanced through them). Create an entry so
                                // the flush check below can run.
                                pending_checkpoints.insert(
                                    subscriber_id.clone(),
                                    PendingCheckpoint::new(new_contiguous, checkpoint_event_id),
                                );
                            }

                            if let Some(pending) = pending_checkpoints.get(&subscriber_id)
                                && should_flush_checkpoint(pending, &config.checkpoint_mode)
                            {
                                let pending = pending_checkpoints.remove(&subscriber_id).unwrap();
                                if let Err(e) = flush_checkpoint(
                                    &checkpoint_pool,
                                    &subscriber_id,
                                    &pending,
                                    &mut checkpoint_cache,
                                )
                                .await
                                {
                                    error!(
                                        "Failed to flush checkpoint for '{}': {}",
                                        subscriber_id, e
                                    );
                                    pending_checkpoints.insert(subscriber_id.clone(), pending);
                                }
                            }
                        }

                        // Break if the batch wasn't full (caught up to the DB) OR if
                        // no new events were processed this iteration. The latter
                        // guards against an infinite hot loop: when a gap blocks
                        // checkpoint advancement but the batch was full (events above
                        // the gap already in processed_ahead), looping would just
                        // re-query the same rows and skip them all again.
                        if !batch_was_full || !processed_any {
                            break;
                        }
                    }
                }
            }
        });

        // Store the handle and shutdown sender
        {
            let mut state = self.listener_state.lock().await;
            *state = Some(ListenerState {
                handle,
                shutdown_tx,
            });
        }

        Ok(())
    }

    /// Gracefully shuts down the event bus listener.
    ///
    /// This method:
    /// 1. Signals the listener to stop accepting new events
    /// 2. Flushes any pending checkpoints (important for `Batched` mode)
    /// 3. Waits for the listener task to complete
    ///
    /// # Returns
    ///
    /// - `Ok(())` if shutdown completed successfully
    /// - `Err` if the listener was not started or if the task panicked
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// // Start the listener
    /// event_bus.start_listener().await?;
    ///
    /// // ... process events ...
    ///
    /// // Graceful shutdown
    /// event_bus.shutdown().await?;
    /// ```
    pub async fn shutdown(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let state = {
            let mut guard = self.listener_state.lock().await;
            guard.take()
        };

        match state {
            Some(ListenerState {
                handle,
                shutdown_tx,
            }) => {
                // Signal shutdown
                let _ = shutdown_tx.send(true);

                // Wait for the task to complete
                handle
                    .await
                    .map_err(|e| format!("Listener task panicked: {}", e))?;

                info!("Event bus listener shut down gracefully");
                Ok(())
            }
            None => Err("Listener was not started".into()),
        }
    }

    /// Returns whether the listener is currently running.
    pub async fn is_running(&self) -> bool {
        let state = self.listener_state.lock().await;
        state.is_some()
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
    ///
    /// For production use cases with potentially large DLQs, consider using
    /// [`get_dlq_entries_paginated`](Self::get_dlq_entries_paginated) instead.
    pub async fn get_dlq_entries(&self, subscriber_id: &str) -> Result<Vec<DlqEntry>, SqlxError> {
        let rows = sqlx::query(
            r#"
            SELECT id, subscriber_id, event_id, global_sequence, error_message, retry_count,
                   created_at, last_retry_at, resolved_at, resolved_by, resolution_notes
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
                resolved_at: row.get("resolved_at"),
                resolved_by: row.get("resolved_by"),
                resolution_notes: row.get("resolution_notes"),
            })
            .collect())
    }

    /// Retrieves DLQ entries for a specific subscriber with pagination.
    ///
    /// This method is recommended for production use cases where the DLQ
    /// could contain many entries.
    ///
    /// # Arguments
    ///
    /// * `subscriber_id` - The subscriber to retrieve entries for
    /// * `offset` - Number of entries to skip (for pagination)
    /// * `limit` - Maximum number of entries to return
    ///
    /// # Returns
    ///
    /// A vector of DLQ entries, ordered by creation time (oldest first).
    pub async fn get_dlq_entries_paginated(
        &self,
        subscriber_id: &str,
        offset: u64,
        limit: u64,
    ) -> Result<Vec<DlqEntry>, SqlxError> {
        let rows = sqlx::query(
            r#"
            SELECT id, subscriber_id, event_id, global_sequence, error_message, retry_count,
                   created_at, last_retry_at, resolved_at, resolved_by, resolution_notes
            FROM epoch_event_bus_dlq
            WHERE subscriber_id = $1
            ORDER BY created_at ASC
            OFFSET $2
            LIMIT $3
            "#,
        )
        .bind(subscriber_id)
        .bind(offset as i64)
        .bind(limit as i64)
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
                resolved_at: row.get("resolved_at"),
                resolved_by: row.get("resolved_by"),
                resolution_notes: row.get("resolution_notes"),
            })
            .collect())
    }

    /// Counts the total number of DLQ entries for a specific subscriber.
    ///
    /// Useful for pagination when you need to know the total number of entries.
    pub async fn count_dlq_entries(&self, subscriber_id: &str) -> Result<u64, SqlxError> {
        let result: (i64,) = sqlx::query_as(
            r#"
            SELECT COUNT(*)
            FROM epoch_event_bus_dlq
            WHERE subscriber_id = $1
            "#,
        )
        .bind(subscriber_id)
        .fetch_one(&self.pool)
        .await?;

        Ok(result.0 as u64)
    }

    /// Removes a specific DLQ entry after successful manual reprocessing.
    ///
    /// Call this after you've successfully reprocessed a failed event to remove
    /// it from the dead letter queue.
    ///
    /// # Arguments
    ///
    /// * `subscriber_id` - The subscriber the entry belongs to
    /// * `event_id` - The event ID to remove
    ///
    /// # Returns
    ///
    /// `true` if an entry was removed, `false` if no matching entry was found.
    pub async fn remove_dlq_entry(
        &self,
        subscriber_id: &str,
        event_id: Uuid,
    ) -> Result<bool, SqlxError> {
        let result = sqlx::query(
            r#"
            DELETE FROM epoch_event_bus_dlq
            WHERE subscriber_id = $1 AND event_id = $2
            "#,
        )
        .bind(subscriber_id)
        .bind(event_id)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Removes all DLQ entries for a specific subscriber.
    ///
    /// Use this to clear the DLQ for a subscriber after bulk reprocessing
    /// or when you want to reset the error state.
    ///
    /// # Returns
    ///
    /// The number of entries that were removed.
    pub async fn remove_all_dlq_entries(&self, subscriber_id: &str) -> Result<u64, SqlxError> {
        let result = sqlx::query(
            r#"
            DELETE FROM epoch_event_bus_dlq
            WHERE subscriber_id = $1
            "#,
        )
        .bind(subscriber_id)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected())
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

            // Create a bounded buffer for events arriving during catch-up.
            // The buffer uses a bounded mpsc channel to apply backpressure when full,
            // preventing memory exhaustion during extended catch-up periods.
            let buffer_size = config.catch_up_buffer_size;
            let (buffer_tx, mut buffer_rx) =
                tokio::sync::mpsc::channel::<Event<Self::EventType>>(buffer_size);

            // Start a temporary listener to buffer events during catch-up
            let buffer_listener_pool = pool.clone();
            let buffer_channel = channel_name.clone();

            // WARNING: If the buffer listener fails to connect, events arriving during catch-up
            // may be missed. The catch-up process will still complete, but there's a window
            // where new events could be lost. Monitor for these errors in production.
            //
            // Design Decision: We log errors rather than failing subscribe() because:
            // 1. The catch-up will still process all historical events correctly
            // 2. The main listener (started after catch-up) will handle new events
            // 3. Only events arriving *during* catch-up in a narrow window could be missed
            // 4. Failing subscribe() would prevent the projection from starting at all
            //
            // For stricter guarantees, callers can monitor logs for buffer listener errors
            // and implement their own retry logic around subscribe().
            //
            // Spawn a task to listen and buffer events
            let buffer_handle = tokio::spawn(async move {
                let mut listener = match PgListener::connect_with(&buffer_listener_pool).await {
                    Ok(l) => l,
                    Err(e) => {
                        error!(
                            "Failed to create buffer listener: {}. Events during catch-up may be missed!",
                            e
                        );
                        return;
                    }
                };

                if let Err(e) = listener.listen(&buffer_channel).await {
                    error!(
                        "Failed to listen for buffer: {}. Events during catch-up may be missed!",
                        e
                    );
                    return;
                }

                log::debug!("Buffer listener started for catch-up");

                // Listen until the sender is dropped (when catch-up completes)
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
                                    causation_id: db_event.causation_id,
                                    correlation_id: db_event.correlation_id,
                                };
                                // Send to bounded channel - will apply backpressure if full
                                if buffer_tx.send(event).await.is_err() {
                                    // Receiver dropped, catch-up is complete
                                    break;
                                }
                                log::debug!("Buffered event during catch-up: {:?}", db_event.id);
                            }
                        }
                        Ok(Err(_)) => {
                            // Listener error, stop buffering
                            break;
                        }
                        Err(_) => {
                            // Timeout - check if sender is still connected
                            if buffer_tx.is_closed() {
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
                        global_sequence,
                        causation_id,
                        correlation_id
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
                    let event_global_seq = row.global_sequence.unwrap_or(0) as u64;
                    let event_id = row.id;

                    let data: Option<Self::EventType> = match row
                        .data
                        .map(|d| serde_json::from_value(d))
                        .transpose()
                    {
                        Ok(d) => d,
                        Err(e) => {
                            warn!(
                                "Catch-up: skipping event {} (type: '{}', global_seq: {}) for '{}': \
                                     failed to deserialize: {}. This is expected when event variants have \
                                     been removed from the application enum. Advancing checkpoint past this event.",
                                event_id, row.event_type, event_global_seq, subscriber_id, e
                            );
                            // Advance current_sequence and checkpoint past the
                            // undeserializable event to avoid an infinite retry loop.
                            current_sequence = event_global_seq;
                            total_caught_up += 1;
                            match &mut pending_checkpoint {
                                Some(pending) => {
                                    pending.update(event_global_seq, event_id);
                                }
                                None => {
                                    pending_checkpoint =
                                        Some(PendingCheckpoint::new(event_global_seq, event_id));
                                }
                            }
                            if let Some(ref pending) = pending_checkpoint
                                && should_flush_checkpoint(pending, &config.checkpoint_mode)
                            {
                                let pending = pending_checkpoint.take().unwrap();
                                if let Err(flush_err) = flush_checkpoint(
                                    &pool,
                                    &subscriber_id,
                                    &pending,
                                    &mut checkpoint_cache,
                                )
                                .await
                                {
                                    error!(
                                        "Catch-up: failed to flush checkpoint for '{}': {}",
                                        subscriber_id, flush_err
                                    );
                                    pending_checkpoint = Some(pending);
                                }
                            }
                            continue;
                        }
                    };

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
                        causation_id: row.causation_id,
                        correlation_id: row.correlation_id,
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
                        if let Err(e) =
                            flush_checkpoint(&pool, &subscriber_id, &pending, &mut checkpoint_cache)
                                .await
                        {
                            error!(
                                "Catch-up: failed to flush checkpoint for '{}': {}",
                                subscriber_id, e
                            );
                            // Re-insert pending checkpoint for retry
                            pending_checkpoint = Some(pending);
                        }
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
            if let Some(pending) = pending_checkpoint.take()
                && let Err(e) =
                    flush_checkpoint(&pool, &subscriber_id, &pending, &mut checkpoint_cache).await
            {
                error!(
                    "Catch-up: failed to flush final checkpoint for '{}': {}",
                    subscriber_id, e
                );
            }

            if total_caught_up > 0 {
                info!(
                    "Catch-up complete for '{}': processed {} events, checkpoint now at {}",
                    subscriber_id, total_caught_up, current_sequence
                );
            }

            // Stop the buffer listener
            buffer_handle.abort();

            // Drain and process buffered events with deduplication.
            // Close the receiver to signal the sender to stop.
            buffer_rx.close();
            let mut buffered_events = Vec::new();
            while let Some(event) = buffer_rx.recv().await {
                buffered_events.push(event);
            }

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
                    if let Err(e) =
                        flush_checkpoint(&pool, &subscriber_id, &pending, &mut checkpoint_cache)
                            .await
                    {
                        error!(
                            "Buffer processing: failed to flush checkpoint for '{}': {}",
                            subscriber_id, e
                        );
                        // Re-insert pending checkpoint for retry
                        pending_checkpoint = Some(pending);
                    }
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
            if let Some(pending) = pending_checkpoint.take()
                && let Err(e) =
                    flush_checkpoint(&pool, &subscriber_id, &pending, &mut checkpoint_cache).await
            {
                error!(
                    "Buffer processing: failed to flush final checkpoint for '{}': {}",
                    subscriber_id, e
                );
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

            // Add to live projections for real-time events.
            //
            // RACE CONDITION NOTE: Between stopping the buffer listener and adding to live
            // projections, the main listener may receive events. This is safe because:
            // 1. We flush the checkpoint before adding to live projections
            // 2. The main listener checks checkpoints before processing each event
            // 3. Events with sequence <= checkpoint are skipped as duplicates
            //
            // The only edge case is if checkpoint flush fails above - in that case,
            // events may be reprocessed when the subscriber restarts, which is acceptable
            // for at-least-once delivery semantics.
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
            resolved_at: None,
            resolved_by: None,
            resolution_notes: None,
        };

        assert_eq!(entry.subscriber_id, "projection:test");
        assert_eq!(entry.global_sequence, 42);
        assert_eq!(entry.retry_count, 3);
        assert!(entry.error_message.is_some());
        assert!(entry.resolved_at.is_none());
    }
}
