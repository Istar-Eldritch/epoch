//! In-memory snapshot store for testing.

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use epoch_core::snapshot::{Snapshot, SnapshotRetention, SnapshotStore};
use tokio::sync::Mutex;
use uuid::Uuid;

/// Error type for [`InMemorySnapshotStore`]. Currently infallible.
#[derive(Debug, thiserror::Error)]
pub enum InMemorySnapshotStoreError {}

/// In-memory implementation of [`SnapshotStore`], primarily for testing.
///
/// Backed by `Arc<Mutex<HashMap>>` so it can be shared and cloned cheaply.
/// Requires `S: Clone + Send + Sync`.
#[derive(Clone, Debug)]
pub struct InMemorySnapshotStore<S>(Arc<Mutex<HashMap<Uuid, Vec<Snapshot<S>>>>>)
where
    S: Clone + std::fmt::Debug;

impl<S> Default for InMemorySnapshotStore<S>
where
    S: Clone + std::fmt::Debug,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<S> InMemorySnapshotStore<S>
where
    S: Clone + std::fmt::Debug,
{
    /// Creates a new, empty `InMemorySnapshotStore`.
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(HashMap::new())))
    }
}

#[async_trait]
impl<S> SnapshotStore<S> for InMemorySnapshotStore<S>
where
    S: Clone + Send + Sync + std::fmt::Debug,
{
    type Error = InMemorySnapshotStoreError;

    async fn load_snapshot(
        &self,
        stream_id: Uuid,
        target_version: u64,
    ) -> Result<Option<Snapshot<S>>, Self::Error> {
        let store = self.0.lock().await;
        let result = store
            .get(&stream_id)
            .and_then(|snaps| {
                // Vec is kept sorted ascending; find the last entry with version <= target
                snaps.iter().rev().find(|s| s.version <= target_version)
            })
            .cloned();
        Ok(result)
    }

    async fn save_snapshot(
        &self,
        stream_id: Uuid,
        version: u64,
        state: &S,
    ) -> Result<(), Self::Error> {
        let mut store = self.0.lock().await;
        let snaps = store.entry(stream_id).or_default();
        match snaps.iter().position(|s| s.version == version) {
            Some(idx) => {
                snaps[idx].state = state.clone();
            }
            None => {
                snaps.push(Snapshot {
                    version,
                    state: state.clone(),
                });
                // Keep sorted by version ascending.
                snaps.sort_unstable_by_key(|s| s.version);
            }
        }
        Ok(())
    }

    async fn apply_retention(
        &self,
        stream_id: Uuid,
        policy: &SnapshotRetention,
    ) -> Result<(), Self::Error> {
        match policy {
            SnapshotRetention::Unlimited => {}
            SnapshotRetention::KeepLast(n) => {
                let n = *n as usize;
                let mut store = self.0.lock().await;
                if let Some(snaps) = store.get_mut(&stream_id) {
                    let len = snaps.len();
                    if len > n {
                        // snaps is sorted ascending; drop the earliest (len - n) entries
                        snaps.drain(0..len - n);
                    }
                }
            }
            other => {
                log::warn!("unhandled SnapshotRetention variant {other:?}; no pruning applied");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use epoch_core::snapshot::state_at;

    async fn store_with_snapshots() -> InMemorySnapshotStore<u64> {
        let store = InMemorySnapshotStore::new();
        store.save_snapshot(Uuid::nil(), 2, &200).await.unwrap();
        store.save_snapshot(Uuid::nil(), 5, &500).await.unwrap();
        store.save_snapshot(Uuid::nil(), 9, &900).await.unwrap();
        store
    }

    #[tokio::test]
    async fn load_snapshot_nearest_le() {
        let id = Uuid::nil();
        let store = store_with_snapshots().await;

        let s = store.load_snapshot(id, 4).await.unwrap();
        assert_eq!(s.map(|x| x.version), Some(2));

        let s = store.load_snapshot(id, 5).await.unwrap();
        assert_eq!(s.map(|x| x.version), Some(5));

        let s = store.load_snapshot(id, 100).await.unwrap();
        assert_eq!(s.map(|x| x.version), Some(9));

        let s = store.load_snapshot(id, 1).await.unwrap();
        assert!(s.is_none());
    }

    #[tokio::test]
    async fn save_snapshot_idempotent() {
        let id = Uuid::new_v4();
        let store = InMemorySnapshotStore::new();

        store.save_snapshot(id, 5, &42u64).await.unwrap();
        store.save_snapshot(id, 5, &99u64).await.unwrap();

        let guard = store.0.lock().await;
        let snaps = guard.get(&id).unwrap();
        assert_eq!(snaps.len(), 1, "only one entry for version 5");
        assert_eq!(snaps[0].state, 99, "second write wins");
    }

    #[tokio::test]
    async fn keep_last_prunes_correctly() {
        let id = Uuid::nil();
        let store = store_with_snapshots().await;

        store
            .apply_retention(id, &SnapshotRetention::KeepLast(2))
            .await
            .unwrap();

        let guard = store.0.lock().await;
        let snaps = guard.get(&id).unwrap();
        assert_eq!(snaps.len(), 2);
        assert_eq!(snaps[0].version, 5);
        assert_eq!(snaps[1].version, 9);
    }

    #[tokio::test]
    async fn unlimited_is_noop() {
        let id = Uuid::nil();
        let store = store_with_snapshots().await;

        store
            .apply_retention(id, &SnapshotRetention::Unlimited)
            .await
            .unwrap();

        let guard = store.0.lock().await;
        let snaps = guard.get(&id).unwrap();
        assert_eq!(snaps.len(), 3);
    }

    #[tokio::test]
    async fn keep_last_zero_empties() {
        let id = Uuid::nil();
        let store = store_with_snapshots().await;

        store
            .apply_retention(id, &SnapshotRetention::KeepLast(0))
            .await
            .unwrap();

        let guard = store.0.lock().await;
        let snaps = guard.get(&id).unwrap();
        assert!(snaps.is_empty());
    }

    #[tokio::test]
    async fn keep_last_larger_than_count_leaves_all() {
        let id = Uuid::nil();
        let store = store_with_snapshots().await;

        store
            .apply_retention(id, &SnapshotRetention::KeepLast(10))
            .await
            .unwrap();

        let guard = store.0.lock().await;
        let snaps = guard.get(&id).unwrap();
        assert_eq!(snaps.len(), 3);
    }

    #[tokio::test]
    async fn load_from_unknown_stream_returns_none() {
        let store = InMemorySnapshotStore::<u64>::new();
        let result = store.load_snapshot(Uuid::new_v4(), 10).await.unwrap();
        assert!(result.is_none());
    }

    // ── state_at equivalence tests ────────────────────────────────────────────
    //
    // These tests verify that state_at(v) returns the same result as a full
    // replay from zero to v, for every v in 1..=N.

    use crate::{InMemoryEventBus, InMemoryEventStore, InMemoryStateStore};
    use epoch_core::{
        event::{EnumConversionError, Event, EventData},
        event_applicator::EventApplicatorState,
        prelude::{EventApplicator, EventStoreBackend},
    };
    use tokio_stream::StreamExt as _;

    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    struct CounterEvent {
        delta: i64,
    }

    impl EventData for CounterEvent {
        fn event_type(&self) -> &'static str {
            "CounterEvent"
        }
    }

    impl TryFrom<&CounterEvent> for CounterEvent {
        type Error = EnumConversionError;
        fn try_from(v: &CounterEvent) -> Result<Self, Self::Error> {
            Ok(v.clone())
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct CounterState {
        id: Uuid,
        version: u64,
        total: i64,
    }

    impl EventApplicatorState for CounterState {
        fn get_id(&self) -> &Uuid {
            &self.id
        }
    }

    #[derive(Debug, thiserror::Error)]
    #[error("counter apply error")]
    struct CounterApplyError;

    struct CounterApplicator {
        id: Uuid,
        state_store: InMemoryStateStore<CounterState>,
    }

    impl CounterApplicator {
        fn new(id: Uuid) -> Self {
            Self {
                id,
                state_store: InMemoryStateStore::new(),
            }
        }
    }

    impl EventApplicator<CounterEvent> for CounterApplicator {
        type State = CounterState;
        type StateStore = InMemoryStateStore<CounterState>;
        type EventType = CounterEvent;
        type ApplyError = CounterApplyError;

        fn get_state_store(&self) -> Self::StateStore {
            self.state_store.clone()
        }

        fn apply(
            &self,
            state: Option<Self::State>,
            event: &Event<Self::EventType>,
        ) -> Result<Option<Self::State>, Self::ApplyError> {
            let delta = event.data.as_ref().map(|d| d.delta).unwrap_or(0);
            let next = match state {
                Some(mut s) => {
                    s.version = event.stream_version;
                    s.total += delta;
                    s
                }
                None => CounterState {
                    id: self.id,
                    version: event.stream_version,
                    total: delta,
                },
            };
            Ok(Some(next))
        }
    }

    fn counter_event(stream_id: Uuid, version: u64, delta: i64) -> Event<CounterEvent> {
        Event::<CounterEvent>::builder()
            .stream_id(stream_id)
            .event_type("CounterEvent".to_string())
            .stream_version(version)
            .data(Some(CounterEvent { delta }))
            .build()
            .unwrap()
    }

    /// Full replay from zero to `target`: calls read_events_since(1) and
    /// truncates at `target`. This is the ground-truth baseline.
    async fn full_replay(
        applicator: &CounterApplicator,
        event_store: &InMemoryEventStore<InMemoryEventBus<CounterEvent>>,
        stream_id: Uuid,
        target: u64,
    ) -> Option<CounterState> {
        use epoch_core::event_store::SliceEventStream;
        use std::pin::Pin;

        let mut raw = event_store.read_events_since(stream_id, 1).await.unwrap();
        let mut collected: Vec<Event<CounterEvent>> = Vec::new();
        while let Some(result) = raw.next().await {
            let ev = result.unwrap();
            if ev.stream_version > target {
                break;
            }
            collected.push(ev);
        }
        let stream: Pin<
            Box<
                dyn epoch_core::event_store::EventStream<
                        CounterEvent,
                        crate::InMemoryEventStoreBackendError,
                    > + Send
                    + '_,
            >,
        > = Box::pin(SliceEventStream::from(collected.as_slice()));
        applicator.re_hydrate(None, stream).await.unwrap()
    }

    #[tokio::test]
    async fn state_at_matches_full_replay_no_snapshots() {
        let stream_id = Uuid::new_v4();
        let bus = InMemoryEventBus::<CounterEvent>::new();
        let event_store = InMemoryEventStore::new(bus);
        let snapshot_store = InMemorySnapshotStore::<CounterState>::new();
        let applicator = CounterApplicator::new(stream_id);

        const N: u64 = 10;
        for v in 1..=N {
            event_store
                .store_event(counter_event(stream_id, v, v as i64))
                .await
                .unwrap();
        }

        for v in 1..=N {
            let via_state_at = state_at(&applicator, &event_store, &snapshot_store, stream_id, v)
                .await
                .unwrap();
            let baseline = full_replay(&applicator, &event_store, stream_id, v).await;
            assert_eq!(via_state_at, baseline, "state_at({v}) != full_replay({v})");
        }
    }

    #[tokio::test]
    async fn state_at_matches_full_replay_with_snapshots() {
        let stream_id = Uuid::new_v4();
        let bus = InMemoryEventBus::<CounterEvent>::new();
        let event_store = InMemoryEventStore::new(bus);
        let snapshot_store = InMemorySnapshotStore::<CounterState>::new();
        let applicator = CounterApplicator::new(stream_id);

        const N: u64 = 15;
        for v in 1..=N {
            event_store
                .store_event(counter_event(stream_id, v, 1))
                .await
                .unwrap();
        }

        // Manually place snapshots at versions 5 and 10.
        let snap5 = full_replay(&applicator, &event_store, stream_id, 5)
            .await
            .unwrap();
        let snap10 = full_replay(&applicator, &event_store, stream_id, 10)
            .await
            .unwrap();
        snapshot_store
            .save_snapshot(stream_id, 5, &snap5)
            .await
            .unwrap();
        snapshot_store
            .save_snapshot(stream_id, 10, &snap10)
            .await
            .unwrap();

        for v in 1..=N {
            let via_state_at = state_at(&applicator, &event_store, &snapshot_store, stream_id, v)
                .await
                .unwrap();
            let baseline = full_replay(&applicator, &event_store, stream_id, v).await;
            assert_eq!(
                via_state_at, baseline,
                "state_at({v}) != full_replay({v}) (with snapshots)"
            );
        }
    }

    #[tokio::test]
    async fn state_at_version_zero_returns_none() {
        let stream_id = Uuid::new_v4();
        let bus = InMemoryEventBus::<CounterEvent>::new();
        let event_store = InMemoryEventStore::new(bus);
        let snapshot_store = InMemorySnapshotStore::<CounterState>::new();
        let applicator = CounterApplicator::new(stream_id);

        // Store some events but request state at version 0.
        event_store
            .store_event(counter_event(stream_id, 1, 1))
            .await
            .unwrap();
        let result = state_at(&applicator, &event_store, &snapshot_store, stream_id, 0)
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn state_at_exact_snapshot_version_returns_snapshot_state() {
        let stream_id = Uuid::new_v4();
        let bus = InMemoryEventBus::<CounterEvent>::new();
        let event_store = InMemoryEventStore::new(bus);
        let snapshot_store = InMemorySnapshotStore::<CounterState>::new();
        let applicator = CounterApplicator::new(stream_id);

        for v in 1..=5u64 {
            event_store
                .store_event(counter_event(stream_id, v, 10))
                .await
                .unwrap();
        }

        let snap = full_replay(&applicator, &event_store, stream_id, 5)
            .await
            .unwrap();
        snapshot_store
            .save_snapshot(stream_id, 5, &snap)
            .await
            .unwrap();

        let result = state_at(&applicator, &event_store, &snapshot_store, stream_id, 5)
            .await
            .unwrap();
        assert_eq!(result, Some(snap));
    }
}
