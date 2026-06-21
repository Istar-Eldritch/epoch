//! PostgreSQL-backed versioned snapshot store.

use async_trait::async_trait;
use epoch_core::snapshot::{Snapshot, SnapshotRetention, SnapshotStore};
use serde::{Serialize, de::DeserializeOwned};
use sqlx::PgPool;
use std::marker::PhantomData;
use uuid::Uuid;

/// PostgreSQL-backed implementation of [`SnapshotStore`].
///
/// Snapshots are stored in the `epoch_snapshots` table created by migration m013.
/// State is serialised as JSONB; `S` must implement [`Serialize`] and
/// [`DeserializeOwned`].
///
/// The table's `PRIMARY KEY (stream_id, version)` makes [`save_snapshot`] idempotent
/// via `ON CONFLICT DO UPDATE`. The same PK serves [`load_snapshot`] via a backward
/// index scan (`ORDER BY version DESC LIMIT 1`).
///
/// [`save_snapshot`]: SnapshotStore::save_snapshot
/// [`load_snapshot`]: SnapshotStore::load_snapshot
#[derive(Clone)]
pub struct PgSnapshotStore<S> {
    pool: PgPool,
    _marker: PhantomData<S>,
}

impl<S> PgSnapshotStore<S> {
    /// Creates a new `PgSnapshotStore` backed by `pool`.
    ///
    /// Migration m013 must have been applied before any store operations are called.
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            _marker: PhantomData,
        }
    }
}

#[async_trait]
impl<S> SnapshotStore<S> for PgSnapshotStore<S>
where
    S: Serialize + DeserializeOwned + Send + Sync,
{
    type Error = sqlx::Error;

    async fn load_snapshot(
        &self,
        stream_id: Uuid,
        target_version: u64,
    ) -> Result<Option<Snapshot<S>>, Self::Error> {
        let row: Option<(i64, serde_json::Value)> = sqlx::query_as(
            r#"
            SELECT version, data
            FROM epoch_snapshots
            WHERE stream_id = $1 AND version <= $2
            ORDER BY version DESC
            LIMIT 1
            "#,
        )
        .bind(stream_id)
        .bind(target_version.min(i64::MAX as u64) as i64)
        .fetch_optional(&self.pool)
        .await?;

        match row {
            None => Ok(None),
            Some((version, data)) => {
                let state: S =
                    serde_json::from_value(data).map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
                Ok(Some(Snapshot {
                    version: version as u64,
                    state,
                }))
            }
        }
    }

    async fn save_snapshot(
        &self,
        stream_id: Uuid,
        version: u64,
        state: &S,
    ) -> Result<(), Self::Error> {
        let data = serde_json::to_value(state).map_err(|e| sqlx::Error::Encode(Box::new(e)))?;
        sqlx::query(
            r#"
            INSERT INTO epoch_snapshots (stream_id, version, data)
            VALUES ($1, $2, $3)
            ON CONFLICT (stream_id, version)
            DO UPDATE SET data = EXCLUDED.data, created_at = NOW()
            "#,
        )
        .bind(stream_id)
        .bind(version as i64)
        .bind(data)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn apply_retention(
        &self,
        stream_id: Uuid,
        policy: &SnapshotRetention,
    ) -> Result<(), Self::Error> {
        match policy {
            SnapshotRetention::Unlimited => Ok(()),
            SnapshotRetention::KeepLast(n) => {
                sqlx::query(
                    r#"
                    DELETE FROM epoch_snapshots
                    WHERE stream_id = $1
                      AND version NOT IN (
                          SELECT version
                          FROM epoch_snapshots
                          WHERE stream_id = $1
                          ORDER BY version DESC
                          LIMIT $2
                      )
                    "#,
                )
                .bind(stream_id)
                .bind(*n as i64)
                .execute(&self.pool)
                .await?;
                Ok(())
            }
            other => {
                log::warn!("unhandled SnapshotRetention variant {other:?}; no pruning applied");
                Ok(())
            }
        }
    }
}
