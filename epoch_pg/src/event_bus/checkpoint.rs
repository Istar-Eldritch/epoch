//! Checkpoint management for reliable event delivery.

use super::config::CheckpointMode;
use log::error;
use sqlx::{Error as SqlxError, PgPool};
use std::collections::HashMap;
use tokio::time::{Duration, Instant};
use uuid::Uuid;

/// Tracks pending checkpoint state for batched checkpointing.
///
/// This struct holds the checkpoint data that hasn't been flushed to the database yet.
#[derive(Debug, Clone)]
pub(crate) struct PendingCheckpoint {
    /// The global sequence to checkpoint
    pub global_sequence: u64,
    /// The event ID to checkpoint
    pub event_id: Uuid,
    /// Number of events processed since last checkpoint write
    pub events_since_checkpoint: u32,
    /// When the first event was processed since last checkpoint write
    pub first_event_time: Instant,
}

impl PendingCheckpoint {
    /// Creates a new pending checkpoint.
    pub fn new(global_sequence: u64, event_id: Uuid) -> Self {
        Self {
            global_sequence,
            event_id,
            events_since_checkpoint: 1,
            first_event_time: Instant::now(),
        }
    }

    /// Updates the pending checkpoint with a new event.
    pub fn update(&mut self, global_sequence: u64, event_id: Uuid) {
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
pub(crate) fn should_flush_checkpoint(pending: &PendingCheckpoint, mode: &CheckpointMode) -> bool {
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
///
/// # Returns
///
/// Returns `Ok(())` if the checkpoint was successfully written, or an error if the
/// database operation failed. Callers should handle errors appropriately based on
/// the checkpoint mode - failures in `Synchronous` mode are more critical than in
/// `Batched` mode.
pub(crate) async fn flush_checkpoint(
    pool: &PgPool,
    subscriber_id: &str,
    pending: &PendingCheckpoint,
    checkpoint_cache: &mut HashMap<String, u64>,
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
    .bind(pending.global_sequence as i64)
    .bind(pending.event_id)
    .execute(pool)
    .await?;

    checkpoint_cache.insert(subscriber_id.to_string(), pending.global_sequence);
    log::debug!(
        "Flushed checkpoint for '{}' to global_sequence {} ({} events batched)",
        subscriber_id,
        pending.global_sequence,
        pending.events_since_checkpoint
    );
    Ok(())
}

/// Flushes all pending checkpoints that have exceeded their max_delay.
///
/// This is called periodically to ensure checkpoints are written even when
/// event rate is low. Errors are logged but not propagated since this runs
/// in a background loop and will be retried on the next interval.
pub(crate) async fn flush_expired_checkpoints(
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
        if let Some(pending) = pending_checkpoints.remove(&subscriber_id)
            && let Err(e) = flush_checkpoint(pool, &subscriber_id, &pending, checkpoint_cache).await
        {
            error!(
                "Failed to flush expired checkpoint for '{}': {}",
                subscriber_id, e
            );
            // Re-insert the pending checkpoint so it can be retried
            pending_checkpoints.insert(subscriber_id, pending);
        }
    }
}

/// Flushes all pending checkpoints to the database.
///
/// This is called before reconnection or shutdown to ensure no checkpoint data is lost.
/// Errors are logged but not propagated since this is typically called during
/// shutdown or reconnection scenarios where we want best-effort persistence.
pub(crate) async fn flush_all_pending_checkpoints(
    pool: &PgPool,
    pending_checkpoints: &mut HashMap<String, PendingCheckpoint>,
    checkpoint_cache: &mut HashMap<String, u64>,
) {
    let subscriber_ids: Vec<String> = pending_checkpoints.keys().cloned().collect();
    for subscriber_id in subscriber_ids {
        if let Some(pending) = pending_checkpoints.remove(&subscriber_id)
            && let Err(e) = flush_checkpoint(pool, &subscriber_id, &pending, checkpoint_cache).await
        {
            error!(
                "Failed to flush checkpoint for '{}' during shutdown/reconnect: {}",
                subscriber_id, e
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_checkpoint_new_initializes_correctly() {
        let event_id = Uuid::new_v4();
        let checkpoint = PendingCheckpoint::new(100, event_id);

        assert_eq!(checkpoint.global_sequence, 100);
        assert_eq!(checkpoint.event_id, event_id);
        assert_eq!(checkpoint.events_since_checkpoint, 1);
    }

    #[test]
    fn pending_checkpoint_update_increments_count() {
        let event_id1 = Uuid::new_v4();
        let event_id2 = Uuid::new_v4();
        let mut checkpoint = PendingCheckpoint::new(100, event_id1);

        checkpoint.update(101, event_id2);

        assert_eq!(checkpoint.global_sequence, 101);
        assert_eq!(checkpoint.event_id, event_id2);
        assert_eq!(checkpoint.events_since_checkpoint, 2);
    }

    #[test]
    fn should_flush_synchronous_always_returns_true() {
        let checkpoint = PendingCheckpoint::new(100, Uuid::new_v4());
        assert!(should_flush_checkpoint(
            &checkpoint,
            &CheckpointMode::Synchronous
        ));
    }

    #[test]
    fn should_flush_batched_returns_true_at_batch_size() {
        let mut checkpoint = PendingCheckpoint::new(100, Uuid::new_v4());
        let mode = CheckpointMode::batched(3, Duration::from_secs(60));

        // Before threshold
        assert!(!should_flush_checkpoint(&checkpoint, &mode));

        // Add more events
        checkpoint.update(101, Uuid::new_v4());
        assert!(!should_flush_checkpoint(&checkpoint, &mode));

        // At threshold
        checkpoint.update(102, Uuid::new_v4());
        assert!(should_flush_checkpoint(&checkpoint, &mode));
    }

    #[test]
    fn should_flush_batched_returns_true_at_max_delay() {
        let mode = CheckpointMode::batched(100, Duration::from_millis(10));
        let mut checkpoint = PendingCheckpoint::new(100, Uuid::new_v4());

        // Should not flush immediately
        assert!(!should_flush_checkpoint(&checkpoint, &mode));

        // Manually set the time in the past
        checkpoint.first_event_time = Instant::now() - Duration::from_millis(15);

        // Should flush after delay
        assert!(should_flush_checkpoint(&checkpoint, &mode));
    }

    #[test]
    fn should_flush_batched_returns_false_before_thresholds() {
        let mode = CheckpointMode::batched(10, Duration::from_secs(60));
        let checkpoint = PendingCheckpoint::new(100, Uuid::new_v4());

        // Single event, well before both thresholds
        assert!(!should_flush_checkpoint(&checkpoint, &mode));
    }
}
