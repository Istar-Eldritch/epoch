//! Versioned snapshot store — config value types, `SnapshotStore` trait.

use crate::aggregate::{Aggregate, AggregateState};
use crate::event::EventData;
use crate::event_applicator::EventApplicator;
use crate::state_store::StateStoreBackend;
use async_trait::async_trait;
use uuid::Uuid;

/// Configures snapshot capture and retention for an aggregate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotConfig {
    /// When automatic snapshots are taken.
    pub trigger: SnapshotTrigger,
    /// How many snapshots to retain per stream.
    pub retention: SnapshotRetention,
}

/// When snapshots are captured.
///
/// Marked `#[non_exhaustive]` so future variants (e.g. `TimeWindow(Duration)`,
/// `PerVersionInterval(u64)`) can be added without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SnapshotTrigger {
    /// No automatic snapshots; the caller invokes `save_snapshot()` explicitly.
    Manual,
    /// Capture a snapshot automatically when an `interval`-version boundary is crossed.
    ///
    /// `interval == 0` is treated as "never" (no automatic capture) and never divides.
    Automatic {
        /// The version interval at which snapshots are automatically captured.
        interval: u64,
    },
}

/// How many snapshots to keep per stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SnapshotRetention {
    /// Keep every snapshot ever taken.
    Unlimited,
    /// Keep only the most recent `n` snapshots per stream (by `version` descending).
    KeepLast(u32),
}

/// A version-keyed historical copy of aggregate state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot<S> {
    /// The `stream_version` this snapshot represents.
    pub version: u64,
    /// The state as of `version`.
    pub state: S,
}

/// A versioned snapshot store: load, save, and prune version-keyed snapshots.
///
/// Implementors add snapshot capabilities on top of the existing
/// [`StateStoreBackend`](crate::state_store::StateStoreBackend), which remains a
/// single-snapshot live-state store unchanged by this trait.
///
/// Serialization requirements are imposed by concrete implementations:
/// `epoch_pg` adds `Serialize + DeserializeOwned`; `epoch_mem` needs only `Clone`.
#[async_trait]
pub trait SnapshotStore<S>: Send + Sync {
    /// The error type for snapshot operations.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Loads the most recent snapshot at or before `target_version`.
    ///
    /// Returns `Ok(None)` if no snapshot exists for `stream_id` at or before the target.
    async fn load_snapshot(
        &self,
        stream_id: Uuid,
        target_version: u64,
    ) -> Result<Option<Snapshot<S>>, Self::Error>;

    /// Saves a snapshot at `version`. **Idempotent** per `(stream_id, version)`:
    /// re-saving the same version overwrites the stored state and must not error.
    async fn save_snapshot(
        &self,
        stream_id: Uuid,
        version: u64,
        state: &S,
    ) -> Result<(), Self::Error>;

    /// Prunes snapshots beyond what `policy` permits, keeping the most recent allowed set.
    ///
    /// A no-op for [`SnapshotRetention::Unlimited`].
    async fn apply_retention(
        &self,
        stream_id: Uuid,
        policy: &SnapshotRetention,
    ) -> Result<(), Self::Error>;
}

/// Errors raised by the manual [`SnapshottingAggregate::save_snapshot`] path.
#[derive(Debug, thiserror::Error)]
pub enum SaveSnapshotError<StateErr, SnapErr> {
    /// The live state could not be read from the state store.
    #[error("Error reading state: {0}")]
    State(StateErr),

    /// No live state exists for the requested aggregate, so nothing can be snapshotted.
    #[error("No state found for aggregate")]
    NoState,

    /// The snapshot store rejected the write.
    #[error("Error saving snapshot: {0}")]
    Snapshot(SnapErr),
}

/// Extension trait adding opt-in snapshot capture and retention to an [`Aggregate`].
///
/// An aggregate implements this trait to wire a [`SnapshotStore`] and a
/// [`SnapshotConfig`] into its command-handling lifecycle. The reusable
/// capture/prune logic lives here as [`capture_snapshot_if_due`](Self::capture_snapshot_if_due);
/// a concrete aggregate bridges it into [`Aggregate::after_persist`] with a one-line
/// override:
///
/// ```ignore
/// async fn after_persist(
///     &self,
///     stream_id: Uuid,
///     new_version: u64,
///     events_applied: usize,
///     state: &MyState,
/// ) {
///     self.capture_snapshot_if_due(stream_id, new_version, events_applied, state)
///         .await;
/// }
/// ```
///
/// Aggregates that do not implement this trait keep the default no-op
/// `after_persist` and are byte-for-byte identical to a non-snapshotting aggregate.
#[async_trait]
pub trait SnapshottingAggregate<ED>: Aggregate<ED>
where
    ED: EventData + Send + Sync + 'static,
    <Self as EventApplicator<ED>>::State: AggregateState,
    Self::CommandData: Send + Sync,
    <Self::Command as TryFrom<Self::CommandData>>::Error: Send + Sync,
{
    /// The snapshot store backing this aggregate.
    type SnapshotStore: SnapshotStore<<Self as EventApplicator<ED>>::State> + Send + Sync;

    /// Returns the snapshot store.
    fn snapshot_store(&self) -> Self::SnapshotStore;

    /// Returns the snapshot capture/retention config.
    fn snapshot_config(&self) -> &SnapshotConfig;

    /// Captures a snapshot and prunes per config, if a capture is due.
    ///
    /// Call this from [`Aggregate::after_persist`]. A snapshot is captured iff the
    /// trigger is [`SnapshotTrigger::Automatic`] with a non-zero `interval` and an
    /// interval boundary falls in `(prev_version, new_version]`, where
    /// `prev_version = new_version - events_applied`. After a successful capture the
    /// configured [`SnapshotRetention`] policy is applied.
    ///
    /// Store failures are logged and swallowed: a snapshot is a rebuildable cache and
    /// must never fail an already-committed command. `interval == 0` never captures
    /// (and never divides).
    async fn capture_snapshot_if_due(
        &self,
        stream_id: Uuid,
        new_version: u64,
        events_applied: usize,
        state: &<Self as EventApplicator<ED>>::State,
    ) {
        let config = self.snapshot_config();
        let interval = match config.trigger {
            SnapshotTrigger::Automatic { interval } if interval > 0 => interval,
            _ => return, // Manual or interval == 0: no automatic capture
        };
        // Capture iff an `interval` boundary lies in (prev_version, new_version].
        let prev = new_version.saturating_sub(events_applied as u64);
        if new_version / interval == prev / interval {
            return; // no boundary crossed this command
        }
        let store = self.snapshot_store();
        if let Err(e) = store.save_snapshot(stream_id, new_version, state).await {
            log::warn!("snapshot capture failed for {stream_id}@{new_version}: {e}");
            return;
        }
        if let Err(e) = store.apply_retention(stream_id, &config.retention).await {
            log::warn!("snapshot retention failed for {stream_id}: {e}");
        }
    }

    /// Manual snapshot path: loads the current live state and persists it as a
    /// versioned snapshot at the state's current version.
    ///
    /// Returns [`SaveSnapshotError::NoState`] if no live state exists for `id`.
    async fn save_snapshot(
        &self,
        id: Uuid,
    ) -> Result<
        (),
        SaveSnapshotError<
            <Self::StateStore as StateStoreBackend<<Self as EventApplicator<ED>>::State>>::Error,
            <Self::SnapshotStore as SnapshotStore<<Self as EventApplicator<ED>>::State>>::Error,
        >,
    > {
        let state = self
            .get_state_store()
            .get_state(id)
            .await
            .map_err(SaveSnapshotError::State)?
            .ok_or(SaveSnapshotError::NoState)?;
        let version = state.get_version();
        self.snapshot_store()
            .save_snapshot(id, version, &state)
            .await
            .map_err(SaveSnapshotError::Snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_config_debug_clone_eq() {
        let cfg = SnapshotConfig {
            trigger: SnapshotTrigger::Automatic { interval: 10 },
            retention: SnapshotRetention::KeepLast(3),
        };
        let cloned = cfg.clone();
        assert_eq!(cfg, cloned);
        // Debug should not panic
        let _ = format!("{cfg:?}");
    }

    #[test]
    fn snapshot_trigger_variants() {
        assert_eq!(SnapshotTrigger::Manual, SnapshotTrigger::Manual);
        assert_ne!(
            SnapshotTrigger::Manual,
            SnapshotTrigger::Automatic { interval: 5 }
        );
        assert_eq!(
            SnapshotTrigger::Automatic { interval: 5 },
            SnapshotTrigger::Automatic { interval: 5 }
        );
        assert_ne!(
            SnapshotTrigger::Automatic { interval: 5 },
            SnapshotTrigger::Automatic { interval: 10 }
        );
    }

    #[test]
    fn snapshot_retention_variants() {
        assert_eq!(SnapshotRetention::Unlimited, SnapshotRetention::Unlimited);
        assert_eq!(
            SnapshotRetention::KeepLast(2),
            SnapshotRetention::KeepLast(2)
        );
        assert_ne!(SnapshotRetention::Unlimited, SnapshotRetention::KeepLast(0));
    }

    #[test]
    fn snapshot_struct_debug_clone_eq() {
        let snap = Snapshot {
            version: 42,
            state: "hello".to_string(),
        };
        let cloned = snap.clone();
        assert_eq!(snap, cloned);
        let _ = format!("{snap:?}");
    }

    /// Verifies the capture-boundary arithmetic used by `SnapshottingAggregate`.
    ///
    /// A snapshot should fire when `new_version / interval != prev / interval`,
    /// i.e. when an interval boundary falls in `(prev_version, new_version]`.
    #[test]
    fn capture_boundary_arithmetic() {
        struct Case {
            interval: u64,
            prev: u64,
            new: u64,
            should_capture: bool,
        }

        let cases = [
            // exact boundary crossing
            Case {
                interval: 5,
                prev: 4,
                new: 5,
                should_capture: true,
            },
            // lands on boundary (multi-event command)
            Case {
                interval: 5,
                prev: 3,
                new: 5,
                should_capture: true,
            },
            // skips over a boundary (command applied many events at once)
            Case {
                interval: 5,
                prev: 3,
                new: 8,
                should_capture: true,
            },
            // no boundary within range
            Case {
                interval: 5,
                prev: 5,
                new: 9,
                should_capture: false,
            },
            // adjacent, no boundary
            Case {
                interval: 5,
                prev: 1,
                new: 2,
                should_capture: false,
            },
            // interval == 0 must never fire (guard prevents divide-by-zero)
            Case {
                interval: 0,
                prev: 0,
                new: 100,
                should_capture: false,
            },
            // single event on boundary
            Case {
                interval: 10,
                prev: 9,
                new: 10,
                should_capture: true,
            },
            // prev == new (zero events applied; degenerate; no boundary)
            Case {
                interval: 5,
                prev: 5,
                new: 5,
                should_capture: false,
            },
        ];

        for c in &cases {
            let fires = if c.interval == 0 {
                false
            } else {
                c.new / c.interval != c.prev / c.interval
            };
            assert_eq!(
                fires, c.should_capture,
                "interval={} prev={} new={} expected={}",
                c.interval, c.prev, c.new, c.should_capture
            );
        }
    }
}
