//! Configuration types for reliable event delivery.

use std::time::Duration;

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

    /// Maximum number of events to buffer during catch-up.
    ///
    /// When a subscriber starts, it catches up from its checkpoint while also
    /// buffering new real-time events. This setting limits the buffer size to
    /// prevent memory exhaustion during extended catch-up periods with high
    /// event rates.
    ///
    /// If the buffer fills up, the real-time listener will apply backpressure
    /// (block) until catch-up drains some events from the buffer.
    ///
    /// Default: 10,000 events
    ///
    /// # Future Improvements
    ///
    /// Consider adding metrics/observability hooks for:
    /// - Buffer utilization percentage during catch-up
    /// - Catch-up duration and event count
    /// - Backpressure events (when buffer fills)
    ///
    /// This would help operators tune these settings for their workloads.
    pub catch_up_buffer_size: usize,
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
            catch_up_buffer_size: 10_000,
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
///
/// # Future Extensions
///
/// A potential third mode (`Partitioned`) could enable horizontal scaling where
/// different instances process different streams (e.g., by `stream_id` hash).
/// This would enable true horizontal scaling while maintaining per-stream
/// ordering guarantees. The `#[non_exhaustive]` attribute allows adding this
/// mode in a future release without breaking existing code.
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
        assert_eq!(config.catch_up_buffer_size, 10_000);
    }

    #[test]
    fn reliable_delivery_config_can_be_customized() {
        let config = ReliableDeliveryConfig {
            max_retries: 5,
            initial_retry_delay: Duration::from_millis(500),
            max_retry_delay: Duration::from_secs(120),
            checkpoint_mode: CheckpointMode::batched(50, Duration::from_secs(10)),
            instance_mode: InstanceMode::Coordinated,
            catch_up_batch_size: 200,
            catch_up_buffer_size: 20_000,
        };

        assert_eq!(config.max_retries, 5);
        assert_eq!(config.initial_retry_delay, Duration::from_millis(500));
        assert_eq!(config.max_retry_delay, Duration::from_secs(120));
        assert_eq!(config.catch_up_batch_size, 200);
        assert_eq!(config.catch_up_buffer_size, 20_000);
    }

    #[test]
    fn reliable_delivery_config_can_use_batched_checkpoint_mode() {
        let config = ReliableDeliveryConfig {
            checkpoint_mode: CheckpointMode::batched(100, Duration::from_secs(5)),
            ..Default::default()
        };

        match config.checkpoint_mode {
            CheckpointMode::Batched {
                batch_size,
                max_delay_ms,
            } => {
                assert_eq!(batch_size, 100);
                assert_eq!(max_delay_ms, 5000);
            }
            _ => panic!("Expected Batched checkpoint mode"),
        }
    }

    #[test]
    fn checkpoint_mode_default_is_synchronous() {
        let mode = CheckpointMode::default();
        assert_eq!(mode, CheckpointMode::Synchronous);
    }

    #[test]
    fn checkpoint_mode_batched_exists_and_differs_from_synchronous() {
        let sync_mode = CheckpointMode::Synchronous;
        let batched_mode = CheckpointMode::batched(100, Duration::from_secs(5));
        assert_ne!(sync_mode, batched_mode);
    }

    #[test]
    fn checkpoint_mode_batched_helper_creates_correct_mode() {
        let mode = CheckpointMode::batched(50, Duration::from_secs(10));
        match mode {
            CheckpointMode::Batched {
                batch_size,
                max_delay_ms,
            } => {
                assert_eq!(batch_size, 50);
                assert_eq!(max_delay_ms, 10_000);
            }
            _ => panic!("Expected Batched mode"),
        }
    }

    #[test]
    fn checkpoint_mode_batched_default_has_expected_values() {
        let mode = CheckpointMode::batched_default();
        match mode {
            CheckpointMode::Batched {
                batch_size,
                max_delay_ms,
            } => {
                assert_eq!(batch_size, 100);
                assert_eq!(max_delay_ms, 5000);
            }
            _ => panic!("Expected Batched mode"),
        }
    }

    #[test]
    fn instance_mode_default_is_single_instance() {
        let mode = InstanceMode::default();
        assert_eq!(mode, InstanceMode::SingleInstance);
    }

    #[test]
    fn instance_mode_coordinated_exists_and_differs_from_single() {
        let single = InstanceMode::SingleInstance;
        let coordinated = InstanceMode::Coordinated;
        assert_ne!(single, coordinated);
    }
}
