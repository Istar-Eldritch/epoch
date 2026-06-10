//! Per-subscriber state tracking for gap-aware contiguous checkpoint advancement.
//!
//! This module provides [`SubscriberState`] which replaces the simple `u64` checkpoint
//! cache with a richer structure that tracks gaps in the global sequence. This enables
//! correct checkpoint advancement even when events are delivered or become visible
//! out of order (e.g., due to PostgreSQL's non-transactional `nextval()` behavior).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::Duration;
use tokio::time::Instant;

/// Describes a single gap that was advanced past due to timeout.
///
/// One entry is returned per sequence the checkpoint skipped because the gap
/// exceeded [`ReliableDeliveryConfig::gap_timeout`]. The caller is responsible
/// for logging, persisting, and invoking any callbacks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkippedGap {
    /// The `global_sequence` that was skipped.
    pub skipped_sequence: u64,
    /// How long the gap was observed before the timeout fired
    /// (`first_seen.elapsed()` at the moment of the skip; always > `gap_timeout`).
    pub gap_duration: Duration,
}

/// Tracks per-subscriber processing state for gap-aware checkpoint advancement.
///
/// Instead of a simple "last seen sequence" counter, this struct maintains:
/// - A **contiguous checkpoint**: the highest sequence where all prior events have been processed
/// - A **processed-ahead set**: events processed beyond the contiguous checkpoint (above a gap)
/// - A **gap tracker**: timestamps for when gaps were first observed, enabling timeout-based resolution
///
/// # Memory Characteristics
///
/// - `processed_ahead`: Empty during normal (in-order) operation. Contains 1–2 entries during
///   brief out-of-order windows. Bounded by the number of concurrent transactions.
/// - `gap_first_seen`: Same size as the number of active gaps. Entries are removed when gaps
///   fill or time out.
#[derive(Debug)]
pub(crate) struct SubscriberState {
    /// The highest contiguous global_sequence that has been processed.
    /// All events with sequence <= this value have been processed or confirmed missing.
    /// This is the value persisted to the database checkpoint.
    pub contiguous_checkpoint: u64,

    /// Global sequences that have been processed but are above `contiguous_checkpoint`.
    /// These are events processed "ahead" of a gap. Bounded by the number of
    /// concurrent uncommitted transactions (typically 0–2 entries).
    pub processed_ahead: HashSet<u64>,

    /// Tracks when gaps were first observed, for timeout-based resolution.
    /// Key: the missing global_sequence. Value: when we first noticed it missing.
    pub gap_first_seen: HashMap<u64, Instant>,
}

impl SubscriberState {
    /// Creates a new `SubscriberState` initialized from a persisted checkpoint.
    ///
    /// # Arguments
    ///
    /// * `checkpoint` - The last persisted contiguous global_sequence for this subscriber.
    ///   Pass `0` if no checkpoint exists yet.
    pub fn new(checkpoint: u64) -> Self {
        Self {
            contiguous_checkpoint: checkpoint,
            processed_ahead: HashSet::new(),
            gap_first_seen: HashMap::new(),
        }
    }
}

/// Advances the contiguous checkpoint as far as possible given the current state.
///
/// Starting from `contiguous_checkpoint + 1`, this function attempts to advance
/// the checkpoint by checking:
///
/// 1. If the next sequence is in `processed_ahead` → advance (event was processed out-of-order)
/// 2. If the next sequence is a gap (not in DB results, not processed) →
///    - If the gap has timed out → advance past it (assumed rolled-back transaction)
///    - If the gap is new → record it in `gap_first_seen` and stop
///    - If the gap is known but not yet timed out → stop and wait
/// 3. If there are no more visible events beyond the current position → stop
///
/// # Arguments
///
/// * `state` - Mutable reference to the subscriber's tracking state
/// * `visible_seqs` - The set of global_sequence numbers returned by the DB query.
///   These are committed, visible events ordered by sequence.
/// * `gap_timeout` - How long to wait before assuming a gap is from a rolled-back transaction.
pub(crate) fn advance_contiguous_checkpoint(
    state: &mut SubscriberState,
    visible_seqs: &BTreeSet<u64>,
    gap_timeout: Duration,
) -> Vec<SkippedGap> {
    let mut skipped = Vec::new();
    loop {
        let next = state.contiguous_checkpoint + 1;

        if state.processed_ahead.contains(&next) {
            // This sequence was processed out-of-order (or skipped due to deser failure)
            state.processed_ahead.remove(&next);
            state.gap_first_seen.remove(&next);
            state.contiguous_checkpoint = next;
            continue;
        }

        // Check if `next` is a gap: there are visible events above `next` but `next` itself
        // is not in the visible set and not in processed_ahead.
        let has_events_above = visible_seqs
            .iter()
            .any(|&seq| seq > state.contiguous_checkpoint);
        if has_events_above && !visible_seqs.contains(&next) {
            // `next` is not visible in the DB and not processed — this is a gap.
            // Either an uncommitted transaction or a rolled-back one.
            if let Some(first_seen) = state.gap_first_seen.get(&next) {
                if first_seen.elapsed() > gap_timeout {
                    // Gap has been observed long enough — assume rolled back
                    let gap_duration = first_seen.elapsed();
                    log::debug!(
                        "Advancing past gap at seq {} (timed out after {:?})",
                        next,
                        gap_duration
                    );
                    skipped.push(SkippedGap {
                        skipped_sequence: next,
                        gap_duration,
                    });
                    state.gap_first_seen.remove(&next);
                    state.contiguous_checkpoint = next;
                    continue;
                }
                // Gap not old enough, wait for it to fill
            } else {
                // First time seeing this gap — record timestamp
                state.gap_first_seen.insert(next, Instant::now());
            }

            break; // Can't advance further — blocked by gap
        }

        // If `next` is in visible_seqs, it hasn't been processed yet
        // (it's not in processed_ahead). We should not advance past unprocessed events.
        // This case means the caller hasn't processed `next` yet — stop.
        break;
    }
    skipped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscriber_state_new_defaults() {
        let state = SubscriberState::new(0);
        assert_eq!(state.contiguous_checkpoint, 0);
        assert!(state.processed_ahead.is_empty());
        assert!(state.gap_first_seen.is_empty());
    }

    #[test]
    fn subscriber_state_new_with_checkpoint() {
        let state = SubscriberState::new(42);
        assert_eq!(state.contiguous_checkpoint, 42);
        assert!(state.processed_ahead.is_empty());
        assert!(state.gap_first_seen.is_empty());
    }

    #[test]
    fn advance_contiguous_no_gaps() {
        // checkpoint=5, processed_ahead={6,7,8}, visible=[6,7,8]
        // Should advance to 8 and return empty Vec
        let mut state = SubscriberState::new(5);
        state.processed_ahead.insert(6);
        state.processed_ahead.insert(7);
        state.processed_ahead.insert(8);

        let visible: BTreeSet<u64> = [6, 7, 8].into_iter().collect();
        let gap_timeout = Duration::from_secs(5);

        let skipped = advance_contiguous_checkpoint(&mut state, &visible, gap_timeout);

        assert_eq!(state.contiguous_checkpoint, 8);
        assert!(state.processed_ahead.is_empty());
        assert!(
            skipped.is_empty(),
            "no gaps timed out, so Vec must be empty"
        );
    }

    #[test]
    fn advance_contiguous_with_gap() {
        // checkpoint=5, visible=[7,8] (6 is missing — gap)
        // processed_ahead is empty (7 and 8 not yet processed by caller)
        // Should stay at 5 and record gap at 6
        let mut state = SubscriberState::new(5);

        let visible: BTreeSet<u64> = [7, 8].into_iter().collect();
        let gap_timeout = Duration::from_secs(5);

        let skipped = advance_contiguous_checkpoint(&mut state, &visible, gap_timeout);

        assert_eq!(state.contiguous_checkpoint, 5);
        assert!(state.gap_first_seen.contains_key(&6));
        assert!(
            skipped.is_empty(),
            "gap not yet timed out, so Vec must be empty"
        );
    }

    #[test]
    fn advance_contiguous_gap_timeout() {
        // checkpoint=5, visible=[7,8], gap at 6 aged past timeout
        // processed_ahead={7,8} (already processed)
        // Should advance past gap at 6, then through 7 and 8 → checkpoint=8
        // Should return Vec with SkippedGap for seq 6
        let mut state = SubscriberState::new(5);
        state.processed_ahead.insert(7);
        state.processed_ahead.insert(8);

        // Simulate gap at 6 that was seen long ago
        state
            .gap_first_seen
            .insert(6, Instant::now() - Duration::from_secs(10));

        let visible: BTreeSet<u64> = [7, 8].into_iter().collect();
        let gap_timeout = Duration::from_secs(5);

        let skipped = advance_contiguous_checkpoint(&mut state, &visible, gap_timeout);

        assert_eq!(state.contiguous_checkpoint, 8);
        assert!(state.processed_ahead.is_empty());
        assert!(!state.gap_first_seen.contains_key(&6));
        // Exactly one gap was skipped
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].skipped_sequence, 6);
        assert!(
            skipped[0].gap_duration >= gap_timeout,
            "gap_duration {:?} should be >= gap_timeout {:?}",
            skipped[0].gap_duration,
            gap_timeout
        );
    }

    #[test]
    fn advance_contiguous_with_processed_ahead() {
        // checkpoint=5, processed_ahead={6}, visible=[6,7,8]
        // 6 is in processed_ahead, so advance to 6
        // 7 is visible but NOT in processed_ahead (not yet processed), stop at 6
        let mut state = SubscriberState::new(5);
        state.processed_ahead.insert(6);

        let visible: BTreeSet<u64> = [6, 7, 8].into_iter().collect();
        let gap_timeout = Duration::from_secs(5);

        let skipped = advance_contiguous_checkpoint(&mut state, &visible, gap_timeout);

        assert_eq!(state.contiguous_checkpoint, 6);
        assert!(!state.processed_ahead.contains(&6));
        assert!(skipped.is_empty());
    }

    #[test]
    fn advance_contiguous_with_processed_ahead_full_chain() {
        // checkpoint=5, processed_ahead={6,7,8}, visible=[6,7,8]
        // All are in processed_ahead → advance to 8
        let mut state = SubscriberState::new(5);
        state.processed_ahead.insert(6);
        state.processed_ahead.insert(7);
        state.processed_ahead.insert(8);

        let visible: BTreeSet<u64> = [6, 7, 8].into_iter().collect();
        let gap_timeout = Duration::from_secs(5);

        let skipped = advance_contiguous_checkpoint(&mut state, &visible, gap_timeout);

        assert_eq!(state.contiguous_checkpoint, 8);
        assert!(state.processed_ahead.is_empty());
        assert!(skipped.is_empty());
    }

    #[test]
    fn advance_contiguous_multiple_gaps() {
        // checkpoint=5, visible=[8,10], processed_ahead={}
        // Gap at 6, 7 — should stay at 5 and record gap at 6
        let mut state = SubscriberState::new(5);

        let visible: BTreeSet<u64> = [8, 10].into_iter().collect();
        let gap_timeout = Duration::from_secs(5);

        let skipped = advance_contiguous_checkpoint(&mut state, &visible, gap_timeout);

        assert_eq!(state.contiguous_checkpoint, 5);
        assert!(state.gap_first_seen.contains_key(&6));
        // 7 is not recorded yet because we stopped at 6
        assert!(skipped.is_empty());
    }

    #[test]
    fn advance_contiguous_gap_not_yet_timed_out() {
        // checkpoint=5, visible=[7,8], gap at 6 seen recently (not timed out)
        // processed_ahead={7,8}
        // Should stay at 5 because gap at 6 hasn't timed out
        let mut state = SubscriberState::new(5);
        state.processed_ahead.insert(7);
        state.processed_ahead.insert(8);
        state.gap_first_seen.insert(6, Instant::now()); // Just now — not timed out

        let visible: BTreeSet<u64> = [7, 8].into_iter().collect();
        let gap_timeout = Duration::from_secs(5);

        let skipped = advance_contiguous_checkpoint(&mut state, &visible, gap_timeout);

        assert_eq!(state.contiguous_checkpoint, 5);
        // 7 and 8 should remain in processed_ahead since we couldn't advance past 6
        assert!(state.processed_ahead.contains(&7));
        assert!(state.processed_ahead.contains(&8));
        assert!(skipped.is_empty(), "gap not timed out, Vec must be empty");
    }

    #[test]
    fn advance_contiguous_empty_visible_set() {
        // checkpoint=5, visible=[], processed_ahead={6}
        // No visible events above checkpoint → should still advance through processed_ahead
        // But wait — with empty visible_seqs, the `has_events_above` check is false for gaps
        // So it tries: next=6, which IS in processed_ahead → advance to 6
        // Then next=7: not in processed_ahead, no events above → break
        let mut state = SubscriberState::new(5);
        state.processed_ahead.insert(6);

        let visible: BTreeSet<u64> = BTreeSet::new();
        let gap_timeout = Duration::from_secs(5);

        let skipped = advance_contiguous_checkpoint(&mut state, &visible, gap_timeout);

        assert_eq!(state.contiguous_checkpoint, 6);
        assert!(state.processed_ahead.is_empty());
        assert!(skipped.is_empty());
    }

    #[test]
    fn advance_contiguous_gap_fills_clears_tracking() {
        // Simulate a gap at 6 that was recorded, then 6 gets processed
        // checkpoint=5, processed_ahead={6, 7}, gap_first_seen has 6
        // visible=[6, 7]
        // Advancing should clear gap_first_seen for 6 (gap filled, not skipped)
        let mut state = SubscriberState::new(5);
        state.processed_ahead.insert(6);
        state.processed_ahead.insert(7);
        state.gap_first_seen.insert(6, Instant::now());

        let visible: BTreeSet<u64> = [6, 7].into_iter().collect();
        let gap_timeout = Duration::from_secs(5);

        let skipped = advance_contiguous_checkpoint(&mut state, &visible, gap_timeout);

        assert_eq!(state.contiguous_checkpoint, 7);
        assert!(!state.gap_first_seen.contains_key(&6));
        assert!(state.processed_ahead.is_empty());
        // Gap filled (not timed out) — no SkippedGap entries
        assert!(skipped.is_empty(), "gap filled normally, Vec must be empty");
    }

    #[test]
    fn advance_contiguous_consecutive_gap_timeouts() {
        // checkpoint=5, visible=[9,10], processed_ahead={9,10}
        // gaps at 6,7,8 all timed out
        // Should advance through all timed-out gaps and then through processed_ahead
        // Returns SkippedGap entries for 6, 7, 8 in ascending order
        let mut state = SubscriberState::new(5);
        state.processed_ahead.insert(9);
        state.processed_ahead.insert(10);

        let old = Instant::now() - Duration::from_secs(10);
        state.gap_first_seen.insert(6, old);
        state.gap_first_seen.insert(7, old);
        state.gap_first_seen.insert(8, old);

        let visible: BTreeSet<u64> = [9, 10].into_iter().collect();
        let gap_timeout = Duration::from_secs(5);

        let skipped = advance_contiguous_checkpoint(&mut state, &visible, gap_timeout);

        assert_eq!(state.contiguous_checkpoint, 10);
        assert!(state.processed_ahead.is_empty());
        assert!(state.gap_first_seen.is_empty());
        // All three gaps should be in the returned Vec
        assert_eq!(skipped.len(), 3);
        let seqs: Vec<u64> = skipped.iter().map(|s| s.skipped_sequence).collect();
        assert_eq!(
            seqs,
            vec![6, 7, 8],
            "skipped sequences should be in ascending order"
        );
        for sg in &skipped {
            assert!(
                sg.gap_duration >= gap_timeout,
                "gap_duration {:?} should be >= gap_timeout {:?}",
                sg.gap_duration,
                gap_timeout
            );
        }
    }
}
