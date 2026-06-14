//! Per-subscriber state tracking for gap-aware contiguous checkpoint advancement.
//!
//! This module provides [`SubscriberState`] which replaces the simple `u64` checkpoint
//! cache with a richer structure that tracks gaps in the global sequence. This enables
//! correct checkpoint advancement even when events are delivered or become visible
//! out of order (e.g., due to PostgreSQL's non-transactional `nextval()` behavior).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::Duration;
use tokio::time::Instant;

/// A point-in-time view of PostgreSQL transaction-id boundaries used to fence
/// gap resolution. Captured once per catch-up batch from the reader's session
/// (PG13+ `pg_current_snapshot()`).
///
/// Both boundaries are **epoch-extended** 64-bit transaction ids and therefore
/// do not wrap inside a cluster's lifetime, so `xmin >= fence_xmax` comparisons
/// are monotonic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TxidSnapshot {
    /// Oldest still-in-flight xid. Every xid `< xmin` has completed (committed
    /// or aborted) and is no longer running anywhere.
    pub xmin: u64,
    /// First not-yet-assigned xid. Every transaction that has begun has
    /// xid `< xmax`.
    pub xmax: u64,
}

/// Why a gap was advanced past.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkipReason {
    /// The snapshot fence proved the gap is permanent: every transaction that was
    /// in-flight when the gap was first observed has completed and the sequence is
    /// still missing (writer aborted, or a burned sequence value). No data loss;
    /// NOT recorded as a gap-timeout.
    FenceCleared,
    /// The `gap_timeout` backstop fired while the fence still held (an old
    /// transaction is pinning `xmin`) or while fencing was unavailable. Potential
    /// data loss; recorded via CLOUD-169 machinery.
    TimeoutBackstop,
}

/// Describes a single gap that was advanced past during checkpoint resolution.
///
/// One entry is returned per sequence the checkpoint skipped, either because the
/// snapshot fence proved it permanent ([`SkipReason::FenceCleared`]) or because
/// the [`ReliableDeliveryConfig::gap_timeout`] backstop fired
/// ([`SkipReason::TimeoutBackstop`]). The caller is responsible for logging,
/// persisting, and invoking any callbacks based on the [`SkipReason`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkippedGap {
    /// The `global_sequence` that was skipped.
    pub skipped_sequence: u64,
    /// How long the gap was observed before it was skipped
    /// (`first_seen.elapsed()` at the moment of the skip).
    pub gap_duration: Duration,
    /// Why the gap was skipped.
    pub reason: SkipReason,
    /// The fence boundary (`xmax` at first observation) that was — or should have
    /// been — used to fence this gap. `None` when no snapshot was ever available
    /// for this gap (fencing disabled/unavailable the whole time it was held).
    pub fence_xmax: Option<u64>,
}

/// Per-gap tracking captured when a gap is first observed.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GapObservation {
    /// Monotonic instant the gap was first seen (drives the backstop timer).
    pub first_seen: Instant,
    /// `xmax` at first observation, or `None` if no snapshot was available then.
    /// The gap is permanent once a later `xmin >= fence_xmax`.
    pub fence_xmax: Option<u64>,
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

    /// Tracks gaps that have been observed, for fence- and timeout-based resolution.
    /// Key: the missing global_sequence. Value: the [`GapObservation`] captured
    /// when the gap was first noticed.
    pub gap_first_seen: HashMap<u64, GapObservation>,
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
/// # Snapshot fencing (CLOUD-180)
///
/// When a `snapshot` is supplied, a gap is only skipped once the fence proves it
/// permanent (`xmin >= fence_xmax`, where `fence_xmax` is the `xmax` captured when
/// the gap was first observed) — meaning every transaction that was in-flight at
/// detection has completed and the sequence is still missing. The `gap_timeout`
/// then acts only as a backstop for cases where the fence never clears (e.g. an
/// abandoned transaction pinning `xmin`).
///
/// When `snapshot` is `None` (fencing disabled or the snapshot query failed), the
/// function degrades to the legacy `gap_timeout`-only behaviour, byte-for-byte.
///
/// This function remains **pure, synchronous, and DB-free**: the snapshot bounds
/// are passed in by the caller, which owns all DB/log/callback side-effects.
///
/// # Arguments
///
/// * `state` - Mutable reference to the subscriber's tracking state
/// * `visible_seqs` - The set of global_sequence numbers returned by the DB query.
///   These are committed, visible events ordered by sequence.
/// * `gap_timeout` - The backstop: how long to hold a gap whose fence never clears
///   before skipping it anyway.
/// * `snapshot` - The current transaction-id snapshot bounds, or `None` when
///   fencing is unavailable/disabled.
pub(crate) fn advance_contiguous_checkpoint(
    state: &mut SubscriberState,
    visible_seqs: &BTreeSet<u64>,
    gap_timeout: Duration,
    snapshot: Option<TxidSnapshot>,
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
            if let Some(observation) = state.gap_first_seen.get_mut(&next) {
                // LAZY BACKFILL: if we had no snapshot at first observation, capture
                // a fence boundary now so this gap can be fenced going forward.
                if observation.fence_xmax.is_none()
                    && let Some(snap) = snapshot
                {
                    observation.fence_xmax = Some(snap.xmax);
                }

                let first_seen = observation.first_seen;
                let fence_xmax = observation.fence_xmax;

                // FENCE (fast path): can we PROVE the gap is permanent? Every txn that
                // was in-flight at detection has finished (xmin >= fence_xmax) and the
                // sequence is still missing, so its writer aborted (or it was burned).
                if let (Some(snap), Some(fence)) = (snapshot, fence_xmax)
                    && snap.xmin >= fence
                {
                    let gap_duration = first_seen.elapsed();
                    log::debug!(
                        "Advancing past gap at seq {} (fence cleared: xmin {} >= fence_xmax {})",
                        next,
                        snap.xmin,
                        fence
                    );
                    skipped.push(SkippedGap {
                        skipped_sequence: next,
                        gap_duration,
                        reason: SkipReason::FenceCleared,
                        fence_xmax: Some(fence),
                    });
                    state.gap_first_seen.remove(&next);
                    state.contiguous_checkpoint = next;
                    continue;
                }

                // BACKSTOP: fence not (yet) cleared, or fencing unavailable. Evaluate
                // elapsed time once so the recorded duration is exactly the value the
                // timeout decision was made on.
                let gap_duration = first_seen.elapsed();
                if gap_duration > gap_timeout {
                    log::debug!(
                        "Advancing past gap at seq {} (timeout backstop after {:?})",
                        next,
                        gap_duration
                    );
                    skipped.push(SkippedGap {
                        skipped_sequence: next,
                        gap_duration,
                        reason: SkipReason::TimeoutBackstop,
                        fence_xmax,
                    });
                    state.gap_first_seen.remove(&next);
                    state.contiguous_checkpoint = next;
                    continue;
                }
                // Gap held: fence still pinned and backstop not yet fired.
            } else {
                // First time seeing this gap — record timestamp and fence boundary.
                state.gap_first_seen.insert(
                    next,
                    GapObservation {
                        first_seen: Instant::now(),
                        fence_xmax: snapshot.map(|s| s.xmax),
                    },
                );
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

    /// Builds a [`GapObservation`] with no fence boundary (legacy / no-snapshot case).
    fn obs(first_seen: Instant) -> GapObservation {
        GapObservation {
            first_seen,
            fence_xmax: None,
        }
    }

    /// Builds a [`GapObservation`] with an explicit fence boundary.
    fn obs_fenced(first_seen: Instant, fence_xmax: u64) -> GapObservation {
        GapObservation {
            first_seen,
            fence_xmax: Some(fence_xmax),
        }
    }

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

        let skipped = advance_contiguous_checkpoint(&mut state, &visible, gap_timeout, None);

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

        let skipped = advance_contiguous_checkpoint(&mut state, &visible, gap_timeout, None);

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
            .insert(6, obs(Instant::now() - Duration::from_secs(10)));

        let visible: BTreeSet<u64> = [7, 8].into_iter().collect();
        let gap_timeout = Duration::from_secs(5);

        let skipped = advance_contiguous_checkpoint(&mut state, &visible, gap_timeout, None);

        assert_eq!(state.contiguous_checkpoint, 8);
        assert!(state.processed_ahead.is_empty());
        assert!(!state.gap_first_seen.contains_key(&6));
        // Exactly one gap was skipped
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].skipped_sequence, 6);
        assert_eq!(skipped[0].reason, SkipReason::TimeoutBackstop);
        assert_eq!(skipped[0].fence_xmax, None);
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

        let skipped = advance_contiguous_checkpoint(&mut state, &visible, gap_timeout, None);

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

        let skipped = advance_contiguous_checkpoint(&mut state, &visible, gap_timeout, None);

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

        let skipped = advance_contiguous_checkpoint(&mut state, &visible, gap_timeout, None);

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
        state.gap_first_seen.insert(6, obs(Instant::now())); // Just now — not timed out

        let visible: BTreeSet<u64> = [7, 8].into_iter().collect();
        let gap_timeout = Duration::from_secs(5);

        let skipped = advance_contiguous_checkpoint(&mut state, &visible, gap_timeout, None);

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

        let skipped = advance_contiguous_checkpoint(&mut state, &visible, gap_timeout, None);

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
        state.gap_first_seen.insert(6, obs(Instant::now()));

        let visible: BTreeSet<u64> = [6, 7].into_iter().collect();
        let gap_timeout = Duration::from_secs(5);

        let skipped = advance_contiguous_checkpoint(&mut state, &visible, gap_timeout, None);

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
        state.gap_first_seen.insert(6, obs(old));
        state.gap_first_seen.insert(7, obs(old));
        state.gap_first_seen.insert(8, obs(old));

        let visible: BTreeSet<u64> = [9, 10].into_iter().collect();
        let gap_timeout = Duration::from_secs(5);

        let skipped = advance_contiguous_checkpoint(&mut state, &visible, gap_timeout, None);

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
            assert_eq!(sg.reason, SkipReason::TimeoutBackstop);
            assert!(
                sg.gap_duration >= gap_timeout,
                "gap_duration {:?} should be >= gap_timeout {:?}",
                sg.gap_duration,
                gap_timeout
            );
        }
    }

    #[test]
    fn fence_cleared_when_xmin_passed_fence_xmax() {
        // checkpoint=5, visible=[7,8], gap at 6. processed_ahead={7,8}.
        // First observation captures fence_xmax = snapshot A's xmax (= 100).
        // Second call with a snapshot whose xmin (>= 100) has passed the fence
        // proves the gap is permanent → FenceCleared, advance to 8.
        let mut state = SubscriberState::new(5);
        state.processed_ahead.insert(7);
        state.processed_ahead.insert(8);

        let visible: BTreeSet<u64> = [7, 8].into_iter().collect();
        let gap_timeout = Duration::from_secs(30);

        // First batch: record the gap with fence_xmax = 100. Held (no skip).
        let snap_a = TxidSnapshot {
            xmin: 90,
            xmax: 100,
        };
        let skipped =
            advance_contiguous_checkpoint(&mut state, &visible, gap_timeout, Some(snap_a));
        assert_eq!(state.contiguous_checkpoint, 5);
        assert!(skipped.is_empty(), "first observation must hold");
        assert_eq!(state.gap_first_seen.get(&6).unwrap().fence_xmax, Some(100));

        // Second batch: xmin has advanced past the fence → gap proven permanent.
        let snap_b = TxidSnapshot {
            xmin: 100,
            xmax: 105,
        };
        let skipped =
            advance_contiguous_checkpoint(&mut state, &visible, gap_timeout, Some(snap_b));

        assert_eq!(state.contiguous_checkpoint, 8);
        assert!(state.processed_ahead.is_empty());
        assert!(!state.gap_first_seen.contains_key(&6));
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].skipped_sequence, 6);
        assert_eq!(skipped[0].reason, SkipReason::FenceCleared);
        assert_eq!(skipped[0].fence_xmax, Some(100));
    }

    #[test]
    fn fence_holds_when_xmin_below_fence_xmax() {
        // gap at 6 with fence_xmax = 100; xmin still below it and elapsed < timeout
        // → held (empty Vec, checkpoint unchanged).
        let mut state = SubscriberState::new(5);
        state.processed_ahead.insert(7);
        state.processed_ahead.insert(8);
        state
            .gap_first_seen
            .insert(6, obs_fenced(Instant::now(), 100));

        let visible: BTreeSet<u64> = [7, 8].into_iter().collect();
        let gap_timeout = Duration::from_secs(30);
        let snap = TxidSnapshot {
            xmin: 95,
            xmax: 110,
        };

        let skipped = advance_contiguous_checkpoint(&mut state, &visible, gap_timeout, Some(snap));

        assert_eq!(state.contiguous_checkpoint, 5, "gap must be held");
        assert!(state.gap_first_seen.contains_key(&6));
        assert!(skipped.is_empty());
    }

    #[test]
    fn fence_held_then_backstop_fires() {
        // gap at 6 with fence_xmax = 100; xmin still below it but elapsed > timeout
        // → backstop fires as TimeoutBackstop, carrying fence_xmax.
        let mut state = SubscriberState::new(5);
        state.processed_ahead.insert(7);
        state.processed_ahead.insert(8);
        state
            .gap_first_seen
            .insert(6, obs_fenced(Instant::now() - Duration::from_secs(10), 100));

        let visible: BTreeSet<u64> = [7, 8].into_iter().collect();
        let gap_timeout = Duration::from_secs(5);
        let snap = TxidSnapshot {
            xmin: 95,
            xmax: 110,
        };

        let skipped = advance_contiguous_checkpoint(&mut state, &visible, gap_timeout, Some(snap));

        assert_eq!(state.contiguous_checkpoint, 8);
        assert!(!state.gap_first_seen.contains_key(&6));
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].skipped_sequence, 6);
        assert_eq!(skipped[0].reason, SkipReason::TimeoutBackstop);
        assert_eq!(skipped[0].fence_xmax, Some(100));
    }

    #[test]
    fn fence_xmax_backfilled_when_snapshot_arrives_late() {
        // First observation with snapshot = None stamps fence_xmax = None.
        // A later batch with Some(snapshot) backfills fence_xmax = snap.xmax.
        // Once xmin >= backfilled xmax, the gap resolves as FenceCleared.
        let mut state = SubscriberState::new(5);
        state.processed_ahead.insert(7);
        state.processed_ahead.insert(8);

        let visible: BTreeSet<u64> = [7, 8].into_iter().collect();
        let gap_timeout = Duration::from_secs(30);

        // Batch 1: no snapshot → record gap with fence_xmax = None, held.
        let skipped = advance_contiguous_checkpoint(&mut state, &visible, gap_timeout, None);
        assert!(skipped.is_empty());
        assert_eq!(state.gap_first_seen.get(&6).unwrap().fence_xmax, None);

        // Batch 2: snapshot available but xmin still below xmax → backfill, held.
        let snap_a = TxidSnapshot {
            xmin: 90,
            xmax: 100,
        };
        let skipped =
            advance_contiguous_checkpoint(&mut state, &visible, gap_timeout, Some(snap_a));
        assert!(skipped.is_empty());
        assert_eq!(state.gap_first_seen.get(&6).unwrap().fence_xmax, Some(100));
        assert_eq!(state.contiguous_checkpoint, 5);

        // Batch 3: xmin passes the backfilled fence → FenceCleared.
        let snap_b = TxidSnapshot {
            xmin: 100,
            xmax: 120,
        };
        let skipped =
            advance_contiguous_checkpoint(&mut state, &visible, gap_timeout, Some(snap_b));
        assert_eq!(state.contiguous_checkpoint, 8);
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].reason, SkipReason::FenceCleared);
        assert_eq!(skipped[0].fence_xmax, Some(100));
    }

    #[test]
    fn fence_snapshot_never_available_resolves_via_backstop() {
        // All batches pass snapshot = None; the gap never gets a fence_xmax.
        // After gap_timeout elapses it resolves as TimeoutBackstop with fence_xmax = None.
        let mut state = SubscriberState::new(5);
        state.processed_ahead.insert(7);
        state.processed_ahead.insert(8);

        let visible: BTreeSet<u64> = [7, 8].into_iter().collect();
        let gap_timeout = Duration::from_secs(5);

        // Batch 1: record the gap (held, no snapshot).
        let skipped = advance_contiguous_checkpoint(&mut state, &visible, gap_timeout, None);
        assert!(skipped.is_empty());
        assert_eq!(state.gap_first_seen.get(&6).unwrap().fence_xmax, None);

        // Age the gap past the timeout.
        state.gap_first_seen.get_mut(&6).unwrap().first_seen =
            Instant::now() - Duration::from_secs(10);

        // Batch 2: still no snapshot → backstop fires.
        let skipped = advance_contiguous_checkpoint(&mut state, &visible, gap_timeout, None);
        assert_eq!(state.contiguous_checkpoint, 8);
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].reason, SkipReason::TimeoutBackstop);
        assert_eq!(skipped[0].fence_xmax, None);
    }

    #[test]
    fn fence_gap_fills_normally_no_skip() {
        // With fencing active, a gap whose writer commits enters processed_ahead
        // and fills normally — no SkippedGap of any reason.
        let mut state = SubscriberState::new(5);
        state.processed_ahead.insert(6);
        state.processed_ahead.insert(7);
        state
            .gap_first_seen
            .insert(6, obs_fenced(Instant::now(), 100));

        let visible: BTreeSet<u64> = [6, 7].into_iter().collect();
        let gap_timeout = Duration::from_secs(30);
        let snap = TxidSnapshot {
            xmin: 95,
            xmax: 110,
        };

        let skipped = advance_contiguous_checkpoint(&mut state, &visible, gap_timeout, Some(snap));

        assert_eq!(state.contiguous_checkpoint, 7);
        assert!(!state.gap_first_seen.contains_key(&6));
        assert!(state.processed_ahead.is_empty());
        assert!(skipped.is_empty(), "gap filled normally, Vec must be empty");
    }
}
