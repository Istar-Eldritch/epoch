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
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
