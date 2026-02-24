//! Retry logic and error handling for event processing.

use super::config::ReliableDeliveryConfig;
use epoch_core::event::{Event, EventData};
use epoch_core::prelude::EventObserver;
use log::{error, info, warn};
use sqlx::PgPool;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Duration, sleep};

/// Result of processing an event with retry logic.
#[derive(Debug)]
pub(crate) enum ProcessResult {
    /// Event was processed successfully
    Success,
    /// Event failed after all retries and was sent to DLQ
    SentToDlq,
}

/// Calculates the retry delay using exponential backoff with jitter.
///
/// The delay doubles with each attempt, starting from `initial_retry_delay`,
/// and is capped at `max_retry_delay`. A ±10% random jitter is applied to
/// prevent thundering herd problems when multiple subscribers fail on the
/// same event.
///
/// # Arguments
///
/// * `config` - The reliability configuration containing delay settings
/// * `attempt` - The attempt number (0-indexed)
///
/// # Returns
///
/// The delay duration before the next retry attempt, with jitter applied.
pub(crate) fn calculate_retry_delay(config: &ReliableDeliveryConfig, attempt: u32) -> Duration {
    calculate_retry_delay_with_jitter(config, attempt, true)
}

/// Internal function that calculates retry delay with optional jitter.
///
/// This is separated to allow deterministic testing without jitter.
fn calculate_retry_delay_with_jitter(
    config: &ReliableDeliveryConfig,
    attempt: u32,
    apply_jitter: bool,
) -> Duration {
    use rand::Rng;

    let initial_ms = config.initial_retry_delay.as_millis() as u64;
    let max_ms = config.max_retry_delay.as_millis() as u64;

    // Use saturating operations to prevent overflow
    // Cap attempt at 63 to prevent 2^attempt from overflowing u64
    let capped_attempt = attempt.min(63);
    let multiplier = 2u64.saturating_pow(capped_attempt);
    let base_delay_ms = initial_ms.saturating_mul(multiplier).min(max_ms);

    if !apply_jitter || base_delay_ms == 0 {
        return Duration::from_millis(base_delay_ms);
    }

    // Apply ±10% jitter to prevent thundering herd
    let jitter_range = (base_delay_ms as f64 * 0.1) as u64;
    if jitter_range == 0 {
        return Duration::from_millis(base_delay_ms);
    }

    let mut rng = rand::thread_rng();
    let jitter: i64 = rng.gen_range(-(jitter_range as i64)..=(jitter_range as i64));
    let final_delay_ms = (base_delay_ms as i64 + jitter).max(1) as u64;

    Duration::from_millis(final_delay_ms.min(max_ms))
}

/// Calculates the base retry delay without jitter (for testing).
///
/// This function is useful for deterministic testing where jitter would
/// make assertions unreliable.
#[cfg(test)]
pub fn calculate_retry_delay_no_jitter(config: &ReliableDeliveryConfig, attempt: u32) -> Duration {
    calculate_retry_delay_with_jitter(config, attempt, false)
}

/// Processes an event with retry logic and DLQ fallback.
///
/// This is a helper function used by both real-time event processing and catch-up.
/// It attempts to process the event, retrying with exponential backoff on failure,
/// and inserts into the DLQ if all retries are exhausted.
///
/// Returns whether the event was processed successfully or sent to DLQ.
///
/// # Future Improvements
///
/// Consider making retry behavior configurable per-subscriber rather than globally.
/// Some projections may want aggressive retries (critical data) while others prefer
/// fast-fail. This could be achieved by adding an optional `RetryPolicy` to the
/// `EventObserver` trait with a default implementation.
pub(crate) async fn process_event_with_retry<ED>(
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
    let retry_count = config.max_retries + 1;
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
    .bind(retry_count as i32)
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
            event_id, subscriber_id, retry_count
        );

        // Invoke the optional DLQ callback after successful persistence
        if let Some(callback) = &config.on_dlq_insertion {
            let info = super::config::DlqInsertionInfo {
                subscriber_id: subscriber_id.to_string(),
                event_id,
                global_sequence: event_global_seq,
                error_message: error_message.clone(),
                retry_count,
            };
            callback.on_dlq_insertion(info).await;
        }
    }

    ProcessResult::SentToDlq
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_delay_calculation_exponential_backoff() {
        let config = ReliableDeliveryConfig {
            initial_retry_delay: Duration::from_secs(1),
            max_retry_delay: Duration::from_secs(60),
            ..Default::default()
        };

        let delay0 = calculate_retry_delay_no_jitter(&config, 0);
        let delay1 = calculate_retry_delay_no_jitter(&config, 1);
        let delay2 = calculate_retry_delay_no_jitter(&config, 2);

        assert_eq!(delay0, Duration::from_secs(1)); // 1 * 2^0 = 1
        assert_eq!(delay1, Duration::from_secs(2)); // 1 * 2^1 = 2
        assert_eq!(delay2, Duration::from_secs(4)); // 1 * 2^2 = 4
    }

    #[test]
    fn retry_delay_handles_large_attempt_numbers() {
        let config = ReliableDeliveryConfig {
            initial_retry_delay: Duration::from_secs(1),
            max_retry_delay: Duration::from_secs(60),
            ..Default::default()
        };

        // Very large attempt number should be capped
        let delay = calculate_retry_delay_no_jitter(&config, 100);
        assert_eq!(delay, Duration::from_secs(60)); // Capped at max
    }

    #[test]
    fn retry_delay_with_jitter_stays_within_bounds() {
        let config = ReliableDeliveryConfig {
            initial_retry_delay: Duration::from_secs(10),
            max_retry_delay: Duration::from_secs(60),
            ..Default::default()
        };

        // Run multiple times to check jitter
        for _ in 0..100 {
            let delay = calculate_retry_delay(&config, 0);
            // Should be 10s ± 10% = 9s to 11s
            assert!(
                delay >= Duration::from_secs(9) && delay <= Duration::from_secs(11),
                "Delay {:?} out of expected range",
                delay
            );
        }
    }

    #[test]
    fn retry_delay_with_millisecond_precision() {
        let config = ReliableDeliveryConfig {
            initial_retry_delay: Duration::from_millis(100),
            max_retry_delay: Duration::from_secs(10),
            ..Default::default()
        };

        let delay0 = calculate_retry_delay_no_jitter(&config, 0);
        let delay1 = calculate_retry_delay_no_jitter(&config, 1);

        assert_eq!(delay0, Duration::from_millis(100)); // 100ms * 2^0
        assert_eq!(delay1, Duration::from_millis(200)); // 100ms * 2^1
    }
}
