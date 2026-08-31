//! This module defines the `PgEventBus` that implements epoch_core::EventBus using PostgreSQL's
//! LISTEN/NOTIFY feature.

mod checkpoint;
mod config;
mod retry;
mod subscriber_state;

pub(crate) use checkpoint::*;
pub use config::{
    CheckpointMode, DispatchMode, DlqCallback, DlqInsertionInfo, GapTimeoutCallback,
    GapTimeoutInfo, HaltCallback, HaltInfo, HaltReason, InstanceMode, ReliableDeliveryConfig,
};
pub(crate) use retry::{
    ProcessResult, invoke_observer_once, panic_payload_message, process_event_with_retry,
};
pub(crate) use subscriber_state::{
    SkipReason, SubscriberState, TxidSnapshot, advance_contiguous_checkpoint,
};

#[cfg(test)]
pub use retry::calculate_retry_delay_no_jitter;

use crate::event_store::PgDBEvent;
use epoch_core::event::{Event, EventData};
use epoch_core::event_store::{EventBus, FailureMode, SubscriptionMode};
use epoch_core::prelude::EventObserver;
use log::{error, info, warn};
use serde::de::DeserializeOwned;
use sqlx::Error as SqlxError;
use sqlx::postgres::{PgListener, PgPool};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};
use tokio::time::{Duration, sleep};
use uuid::Uuid;

use futures::FutureExt;
use futures::future::join_all;
use std::future::Future;
use std::panic::AssertUnwindSafe;

/// Minimal notification payload — identity/wake signal only.
///
/// Mirrors the keys emitted by `epoch_notify_event()` after migration m010.
/// Full event data is always fetched from the database, so only the fields
/// needed to identify which events arrived during catch-up are deserialized.
#[derive(Debug, serde::Deserialize)]
struct NotifyPayload {
    id: Uuid,
    global_sequence: Option<i64>,
}

/// Result of processing a single subscriber against one batch of events.
struct SubscriberBatchOutcome {
    subscriber_id: String,
    state: SubscriberState,
    pending_checkpoint: Option<PendingCheckpoint>,
    last_event_id: Option<Uuid>,
    cached_checkpoint: Option<u64>,
    processed_any: bool,
}

/// Shared, batch-level context passed to every concurrent per-subscriber task.
/// Grouping these fields avoids exceeding the function-argument limit while
/// keeping each `Arc` clone cheap.
struct BatchContext {
    rows: Arc<Vec<PgDBEvent>>,
    visible_seqs: Arc<BTreeSet<u64>>,
    seq_to_id: Arc<HashMap<u64, Uuid>>,
    config: ReliableDeliveryConfig,
    dlq_pool: PgPool,
    checkpoint_pool: PgPool,
    /// Per-subscriber in-memory high-water mark for `ReplayAlways` subscribers.
    /// Live processing routes checkpoint advancement here instead of the
    /// checkpoints table for such a subscriber.
    hwm: Arc<Mutex<HashMap<String, u64>>>,
    /// The transaction-id snapshot captured once for this batch (CLOUD-180), or
    /// `None` when fencing is disabled, no gaps were active, or the snapshot
    /// query failed (graceful timeout-only fallback for the batch).
    snapshot: Option<TxidSnapshot>,
}

/// The pre-per-channel trigger name, kept only so [`PgEventBus::setup_trigger`]
/// can clean it up on deployments that ran an earlier version.
pub(crate) const LEGACY_NOTIFY_TRIGGER: &str = "epoch_event_bus_notify_trigger";

/// Returns this bus's NOTIFY trigger name: the fixed prefix plus a digest of the
/// channel.
///
/// The name has to encode the channel because the channel is otherwise only an
/// *argument* to the trigger function (`EXECUTE FUNCTION
/// epoch_notify_event('<channel>')`). With one shared name per table, a second
/// bus on the same events table either steals the first bus's trigger or, if
/// creation is skipped because a trigger already exists, is left deaf and
/// silently downgraded to the periodic timer tick. A per-channel name lets every
/// bus own its own trigger and coexist: all of them fire, each notifying its own
/// channel.
///
/// The digest is computed by Postgres' built-in `md5()` rather than a Rust hasher
/// so the name is stable across processes and library versions (`DefaultHasher`
/// guarantees neither), without taking on a hash dependency. Truncated to 16 hex
/// characters to stay inside the 63-character identifier limit, and hex-only by
/// construction, so the result never needs quoting or escaping.
pub(crate) async fn notify_trigger_name(pool: &PgPool, channel: &str) -> Result<String, SqlxError> {
    let (name,): (String,) =
        sqlx::query_as("SELECT 'epoch_event_bus_notify_trigger_' || substr(md5($1), 1, 16)")
            .bind(channel)
            .fetch_one(pool)
            .await?;
    Ok(name)
}

/// Reports whether `trigger_name` exists on `table`. Used by Async `subscribe` to
/// warn when delivery would silently depend on the timer tick (R4), and by
/// `ensure_trigger` to avoid redundant DDL on every listener start.
pub(crate) async fn trigger_exists(
    pool: &PgPool,
    table: &str,
    trigger_name: &str,
) -> Result<bool, SqlxError> {
    let (exists,): (bool,) = sqlx::query_as(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM pg_trigger t
            JOIN pg_class c ON c.oid = t.tgrelid
            WHERE t.tgname = $2
              AND c.relname = $1
        )
        "#,
    )
    .bind(table)
    .bind(trigger_name)
    .fetch_one(pool)
    .await?;
    Ok(exists)
}

/// Ensures the `txid` column (CLOUD-180 snapshot fencing) exists on `table` with
/// the correct `pg_current_xact_id()` default and a partial index.
///
/// Idempotent — safe to call on every startup. The three statements all use
/// `IF NOT EXISTS` / `SET DEFAULT`, so `ADD COLUMN` is a metadata-only operation
/// (no table rewrite) on PG13+. Intended for **custom** events tables; the
/// default `epoch_events` table is covered by migration m011.
///
/// On error the caller should `warn!` and continue: fencing simply degrades to
/// timeout-only for that table.
pub(crate) async fn ensure_txid_column(pool: &PgPool, table: &str) -> Result<(), SqlxError> {
    sqlx::query(&format!(
        "ALTER TABLE {table} ADD COLUMN IF NOT EXISTS txid BIGINT"
    ))
    .execute(pool)
    .await?;
    sqlx::query(&format!(
        "ALTER TABLE {table} ALTER COLUMN txid SET DEFAULT (pg_current_xact_id()::text::bigint)"
    ))
    .execute(pool)
    .await?;
    sqlx::query(&format!(
        "CREATE INDEX IF NOT EXISTS idx_{table}_txid ON {table} (txid) WHERE txid IS NOT NULL"
    ))
    .execute(pool)
    .await?;
    Ok(())
}

/// Ensures the `schema_version` column (CLOUD-173 upcasting) exists on `table`
/// with the correct `DEFAULT 1`.
///
/// Idempotent — safe to call on every startup. Both statements use
/// `IF NOT EXISTS` / `SET DEFAULT`, so `ADD COLUMN` is a metadata-only
/// operation (no table rewrite). Intended for **custom** events tables; the
/// default `epoch_events` table is covered by migration m012.
///
/// On error the caller should `warn!` and continue: schema-version stamping
/// simply degrades to the `NULL` → `1` fallback for that table.
pub(crate) async fn ensure_schema_version_column(
    pool: &PgPool,
    table: &str,
) -> Result<(), SqlxError> {
    sqlx::query(&format!(
        "ALTER TABLE {table} ADD COLUMN IF NOT EXISTS schema_version INT"
    ))
    .execute(pool)
    .await?;
    sqlx::query(&format!(
        "ALTER TABLE {table} ALTER COLUMN schema_version SET DEFAULT 1"
    ))
    .execute(pool)
    .await?;
    Ok(())
}

/// Queries the reader session's current transaction-id snapshot bounds (PG13+).
///
/// Returns `None` (with a single `warn!`) on query failure so the caller can
/// gracefully fall back to timeout-only gap resolution for the batch.
async fn query_txid_snapshot(pool: &PgPool) -> Option<TxidSnapshot> {
    let result = sqlx::query_as::<_, (i64, i64)>(
        "SELECT pg_snapshot_xmin(s)::text::bigint AS xmin, \
         pg_snapshot_xmax(s)::text::bigint AS xmax \
         FROM pg_current_snapshot() s",
    )
    .fetch_one(pool)
    .await;
    match result {
        Ok((xmin, xmax)) => Some(TxidSnapshot {
            xmin: xmin as u64,
            xmax: xmax as u64,
        }),
        Err(e) => {
            warn!(
                "Failed to query txid snapshot for gap fencing; \
                 falling back to timeout-only resolution for this batch: {}",
                e
            );
            None
        }
    }
}

/// Writes (or refreshes) the fail-closed deserialize-halt DLQ row for one event
/// (spec 0028 Q3). Unlike the observer-exhaustion DLQ row, this carries the
/// `unrecoverable: deserialize:` prefix and does not fire `on_dlq_insertion` —
/// the halt observability signal for the deser path is `on_halt` plus the row
/// itself. Failure to write the row is logged, never fatal.
async fn insert_deser_halt_dlq_row(
    dlq_pool: &PgPool,
    subscriber_id: &str,
    event_id: Uuid,
    global_sequence: u64,
    error_message: &str,
) {
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
    .bind(global_sequence as i64)
    .bind(error_message)
    .bind(1i32)
    .execute(dlq_pool)
    .await
    {
        error!(
            "Failed to insert deserialize-halt DLQ row for '{}' (event {}): {}",
            subscriber_id, event_id, e
        );
    }
}

/// Fires the optional `on_halt` callback on halt entry (spec 0028 Q3). Fired
/// once when a fail-closed subscriber enters a halt, never per re-attempt.
async fn fire_on_halt(
    config: &ReliableDeliveryConfig,
    subscriber_id: &str,
    held_below_sequence: u64,
    reason: HaltReason,
) {
    if let Some(callback) = &config.on_halt {
        callback
            .on_halt(HaltInfo {
                subscriber_id: subscriber_id.to_string(),
                held_below_sequence,
                reason,
            })
            .await;
    }
}

/// Processes one subscriber against the pre-fetched batch of events.
/// All per-subscriber state is owned, making this safe to run concurrently.
async fn process_subscriber_for_batch<D>(
    projection: Arc<Mutex<dyn EventObserver<D>>>,
    subscriber_id: String,
    mut state: SubscriberState,
    mut pending_checkpoint: Option<PendingCheckpoint>,
    last_event_id_in: Option<Uuid>,
    ctx: BatchContext,
) -> SubscriberBatchOutcome
where
    D: EventData + Send + Sync + 'static,
{
    let BatchContext {
        rows,
        visible_seqs,
        seq_to_id,
        config,
        dlq_pool,
        checkpoint_pool,
        snapshot,
        hwm,
    } = ctx;
    // R5: a ReplayAlways subscriber advances its in-memory HWM instead of the
    // persisted checkpoint (§4.5).
    let replay_always =
        { projection.lock().await.subscription_mode() == SubscriptionMode::ReplayAlways };
    let contiguous_before = state.contiguous_checkpoint;
    let failure_mode = state.failure_mode;
    let mut processed_any = false;
    let mut last_event_id = last_event_id_in;

    // Bookkeeping for an event this subscriber applied (or, on a fail-open deser
    // skip, advanced past): record it out-of-order and count it toward the
    // `Batched` threshold, but never let an ahead-of-gap event become the
    // published position (spec 0027 R1). The eager seed is born at the current
    // contiguous prefix.
    macro_rules! record_applied {
        ($seq:expr, $id:expr) => {{
            state.processed_ahead.insert($seq);
            last_event_id = Some($id);
            pending_checkpoint
                .get_or_insert_with(|| {
                    PendingCheckpoint::seeded(
                        state.contiguous_checkpoint,
                        state.contiguous_event_id,
                    )
                })
                .record_processed();
            processed_any = true;
        }};
    }

    for row in rows.iter() {
        let event_seq = row.global_sequence.unwrap_or(0) as u64;
        let event_id = row.id;

        if event_seq <= contiguous_before {
            continue;
        }

        if state.processed_ahead.contains(&event_seq) {
            last_event_id = Some(event_id);
            continue;
        }

        // A fail-closed subscriber that halted on exactly this sequence in an
        // earlier cycle re-attempts it with a SINGLE observer invocation and no
        // retry ladder (spec 0028 §3.4).
        let is_reattempt = state.held_event == Some(event_seq);

        let data = match row
            .data
            .clone()
            .map(|d| serde_json::from_value::<D>(d))
            .transpose()
        {
            Ok(data) => data,
            Err(e) => match failure_mode {
                FailureMode::FailClosed => {
                    // Halt: hold the contiguous prefix below this sequence (no
                    // processed_ahead insert, no record_processed), write a DLQ
                    // row, and stop consuming this subscriber's batch. on_halt
                    // fires only on halt ENTRY, never per re-attempt, to avoid
                    // alert spam (spec 0028 §3.4). The DLQ row is likewise only
                    // written on halt entry — a re-attempt must stay cheap, so
                    // it re-holds with a debug log and no DB write (parity with
                    // the observer re-attempt path).
                    let error_message = format!("unrecoverable: deserialize: {}", e);
                    if !is_reattempt {
                        insert_deser_halt_dlq_row(
                            &dlq_pool,
                            &subscriber_id,
                            event_id,
                            event_seq,
                            &error_message,
                        )
                        .await;
                    }
                    if is_reattempt {
                        log::debug!(
                            "Fail-closed subscriber '{}' still cannot deserialize held \
                             event {} (seq {}): {}. Holding.",
                            subscriber_id,
                            event_id,
                            event_seq,
                            e
                        );
                    } else {
                        warn!(
                            "Fail-closed halt for '{}': event {} (seq {}) is undeserializable: \
                             {}. Holding the checkpoint below this sequence until the payload \
                             is corrected or the subscriber is released.",
                            subscriber_id, event_id, event_seq, e
                        );
                        fire_on_halt(
                            &config,
                            &subscriber_id,
                            event_seq,
                            HaltReason::DeserializeFailure,
                        )
                        .await;
                    }
                    state.held_event = Some(event_seq);
                    break;
                }
                // Fail-open (and any future non-fail-closed mode): log, skip,
                // and advance past the undeserializable event (unchanged).
                _ => {
                    warn!(
                        "Skipping event {} (type: '{}', global_seq: {}) \
                         for '{}': failed to deserialize: {}. \
                         This is expected when event variants have been \
                         removed. Advancing checkpoint past this event.",
                        event_id, row.event_type, event_seq, subscriber_id, e
                    );
                    record_applied!(event_seq, event_id);
                    continue;
                }
            },
        };

        let event = Arc::new(Event::<D> {
            id: event_id,
            stream_id: row.stream_id,
            stream_version: row.stream_version as u64,
            event_type: row.event_type.clone(),
            actor_id: row.actor_id,
            purger_id: row.purger_id,
            data,
            created_at: row.created_at,
            purged_at: row.purged_at,
            global_sequence: Some(event_seq),
            causation_id: row.causation_id,
            correlation_id: row.correlation_id,
            // CLOUD-173: carry the stored schema version through the bus read path.
            // NULL (pre-migration rows) is interpreted as version 1.
            schema_version: row.schema_version.unwrap_or(1).max(0) as u32,
        });

        log::debug!(
            "Applying event {} (seq {}) to '{}'",
            event_id,
            event_seq,
            subscriber_id
        );

        match failure_mode {
            // Fail-closed re-attempt of a held event: a single invocation, no
            // retry ladder. Success clears the marker and resumes normal
            // delivery from here; failure re-holds silently (spec 0028 §3.4).
            FailureMode::FailClosed if is_reattempt => {
                match invoke_observer_once(&projection, &event).await {
                    Ok(()) => {
                        state.held_event = None;
                        record_applied!(event_seq, event_id);
                    }
                    Err(err) => {
                        log::debug!(
                            "Fail-closed re-attempt of held event {} (seq {}) for '{}' \
                             failed: {}. Holding.",
                            event_id,
                            event_seq,
                            subscriber_id,
                            err
                        );
                        break;
                    }
                }
            }
            // Fail-closed first attempt: full retry ladder; on exhaustion, halt
            // after the DLQ row / on_dlq_insertion already fired inside it.
            FailureMode::FailClosed => {
                match process_event_with_retry(
                    &projection,
                    &event,
                    &subscriber_id,
                    &config,
                    &dlq_pool,
                )
                .await
                {
                    ProcessResult::Success => {
                        record_applied!(event_seq, event_id);
                    }
                    ProcessResult::SentToDlq => {
                        fire_on_halt(
                            &config,
                            &subscriber_id,
                            event_seq,
                            HaltReason::ObserverFailure,
                        )
                        .await;
                        state.held_event = Some(event_seq);
                        break;
                    }
                }
            }
            // Fail-open (and any future non-fail-closed mode): retry then
            // DLQ-and-continue, advancing past the event regardless of outcome
            // (unchanged behaviour).
            _ => {
                process_event_with_retry(&projection, &event, &subscriber_id, &config, &dlq_pool)
                    .await;
                record_applied!(event_seq, event_id);
            }
        }
    }

    // CLOUD-180: thread the per-batch snapshot into the pure resolver. With
    // `None` (fencing disabled/unavailable) this is byte-for-byte the legacy
    // timeout-only resolver.
    let skipped_gaps =
        advance_contiguous_checkpoint(&mut state, &visible_seqs, config.gap_timeout, snapshot);

    // Partition by reason: `FenceCleared` skips are expected, lossless rollbacks
    // (writer aborted / burned sequence) and are only debug-logged. Only
    // `TimeoutBackstop` skips carry potential data loss and are recorded via the
    // CLOUD-169 machinery.
    let (fence_cleared, timeout_backstop): (Vec<_>, Vec<_>) = skipped_gaps
        .into_iter()
        .partition(|gap| gap.reason == SkipReason::FenceCleared);

    // Fence-cleared gaps are proven permanent with no data loss: a single debug
    // line, no WARN / record / callback.
    if !fence_cleared.is_empty() {
        let gaps_summary = fence_cleared
            .iter()
            .map(|gap| format!("seq {} ({:?})", gap.skipped_sequence, gap.gap_duration))
            .collect::<Vec<_>>()
            .join(", ");
        log::debug!(
            "Snapshot fence cleared for '{}' on bus '{}': advancing past {} \
             proven-permanent (rolled-back / burned) sequence(s) — {} — no data loss",
            subscriber_id,
            config.events_table,
            fence_cleared.len(),
            gaps_summary
        );
    }

    // Emit a single batched WARN log summarising all backstop gaps before the
    // per-gap fire-and-forget persistence/callback tasks run. This avoids
    // N×subscriber identical warnings when many sequences time out at once (e.g.
    // after a deployment restart). See CLOUD-109.
    if !timeout_backstop.is_empty() {
        let bus_name = &config.events_table;
        let gaps_summary = timeout_backstop
            .iter()
            .map(|gap| format!("seq {} ({:?})", gap.skipped_sequence, gap.gap_duration))
            .collect::<Vec<_>>()
            .join(", ");
        warn!(
            "Gap timeout: advancing '{}' on bus '{}' past {} missing sequence(s) — {} \
             — if any writing transaction later commits, the event will NOT be \
             delivered to this subscriber (recorded in epoch_event_bus_gap_timeouts)",
            subscriber_id,
            bus_name,
            timeout_backstop.len(),
            gaps_summary
        );

        // Persistent-pin diagnostic (OQ-3): when fencing was active but the fence
        // never cleared (an old transaction is pinning `xmin` past `fence_xmax`),
        // emit a dedicated WARN with the current `xmin`/`fence_xmax` so operators
        // can identify the offending long-running session via pg_stat_activity /
        // pg_prepared_xacts. Skipped when no snapshot was available this batch.
        if let Some(snap) = snapshot {
            for gap in &timeout_backstop {
                if let Some(fence_xmax) = gap.fence_xmax
                    && snap.xmin < fence_xmax
                {
                    warn!(
                        "Gap timeout backstop fired for '{}' on bus '{}' seq {} while the \
                         snapshot fence was still pinned (xmin {} < fence_xmax {}); an \
                         in-flight transaction may still be holding this sequence — inspect \
                         pg_stat_activity / pg_prepared_xacts for the offending session",
                        subscriber_id, bus_name, gap.skipped_sequence, snap.xmin, fence_xmax
                    );
                }
            }
        }
    }

    // For each backstop gap the checkpoint advanced past, fire-and-forget a task
    // that persists the record and invokes the callback. Neither the log nor the
    // DB write gates checkpoint advancement (NFR-1).
    for gap in timeout_backstop {
        let bus_name = config.events_table.clone();
        let sub_id = subscriber_id.clone();

        let pool = checkpoint_pool.clone();
        let cb = config.on_gap_timeout.clone();
        let skipped_sequence = gap.skipped_sequence;
        let gap_duration = gap.gap_duration;
        let bus_name_task = bus_name.clone();
        let sub_id_task = sub_id.clone();

        tokio::spawn(async move {
            let result = sqlx::query(
                r#"
                INSERT INTO epoch_event_bus_gap_timeouts
                    (bus_name, subscriber_id, skipped_sequence, gap_duration_ms)
                VALUES ($1, $2, $3, $4)
                ON CONFLICT (bus_name, subscriber_id, skipped_sequence) DO NOTHING
                "#,
            )
            .bind(&bus_name_task)
            .bind(&sub_id_task)
            .bind(skipped_sequence as i64)
            .bind(gap_duration.as_millis() as i64)
            .execute(&pool)
            .await;

            match result {
                Err(e) => {
                    error!(
                        "Failed to record gap timeout for '{}' on bus '{}' seq {}: {}",
                        sub_id_task, bus_name_task, skipped_sequence, e
                    );
                }
                Ok(insert_result) => {
                    // Only invoke the callback when a NEW record was inserted.
                    // `ON CONFLICT DO NOTHING` reports 0 affected rows when the
                    // gap was already recorded (e.g. re-detection after a restart
                    // before checkpoint flush, or a concurrent instance) — the
                    // callback must fire exactly once per durable record.
                    if insert_result.rows_affected() == 0 {
                        log::debug!(
                            "Gap timeout for '{}' on bus '{}' seq {} already recorded; skipping callback",
                            sub_id_task,
                            bus_name_task,
                            skipped_sequence
                        );
                        return;
                    }
                    if let Some(callback) = cb {
                        let info = GapTimeoutInfo {
                            bus_name: bus_name_task,
                            subscriber_id: sub_id_task,
                            skipped_sequence,
                            gap_duration,
                        };
                        callback.on_gap_timeout(info).await;
                    }
                }
            }
        });
    }

    let new_contiguous = state.contiguous_checkpoint;
    let mut cached_checkpoint = None;

    if new_contiguous > contiguous_before {
        let checkpoint_event_id = seq_to_id
            .get(&new_contiguous)
            .copied()
            .or(last_event_id)
            .unwrap_or_else(Uuid::nil);

        last_event_id = Some(checkpoint_event_id);
        // Keep the paired id current so the next batch's eager seed pairs
        // correctly (spec 0027 §4 Q1).
        state.contiguous_event_id = checkpoint_event_id;

        if replay_always {
            // Route advancement to the in-memory HWM; never touch the
            // checkpoints table for a ReplayAlways subscriber.
            hwm.lock()
                .await
                .insert(subscriber_id.clone(), new_contiguous);
        } else {
            match &mut pending_checkpoint {
                // Move the published position (and its paired id) without
                // re-counting: each processed event was already counted once by
                // `record_processed` at its per-event site, so bumping the
                // counter here would double-count and trip the `Batched`
                // `batch_size` threshold early (caught by the §7.3 exact-value
                // control). `is_publishable()` becomes true because the position
                // now leads the seed.
                Some(p) => {
                    p.advance(new_contiguous, checkpoint_event_id);
                }
                // Publishable advance constructor: reachable when every row hit
                // the `processed_ahead` early-continue and the gap was then
                // closed by `advance_contiguous_checkpoint` (spec 0027 §3.4).
                None => {
                    pending_checkpoint =
                        Some(PendingCheckpoint::new(new_contiguous, checkpoint_event_id));
                }
            }
        }
    }

    // Flush unconditionally (spec 0027 §3.2/§3.4): moved out of the advance
    // branch so a `Batched` `batch_size` crossing can flush even while a hole is
    // held. The publishable predicate in `try_flush_pending_checkpoint` still
    // holds a never-advanced seed back, so this never writes above a hole (R1).
    // Gated on `!replay_always` so a ReplayAlways subscriber writes no row.
    if !replay_always {
        debug_assert!(
            pending_checkpoint
                .as_ref()
                .is_none_or(|p| p.global_sequence <= state.contiguous_checkpoint),
            "live path must never hold a pending above the contiguous prefix (spec 0027 R1/Q3)"
        );
        // Fresh local cache (rather than one shared across concurrent subscriber
        // tasks) purely to read back the flushed value for `cached_checkpoint`;
        // the caller merges it into the listener-level cache once this task's
        // `SubscriberBatchOutcome` is collected. `cached_checkpoint` is set ONLY
        // from a real flush, so it never records a value that was not persisted.
        let mut local_cache = HashMap::new();
        try_flush_pending_checkpoint(
            &mut pending_checkpoint,
            "process_subscriber_for_batch",
            &config.events_table,
            &subscriber_id,
            &config.checkpoint_mode,
            &checkpoint_pool,
            &mut local_cache,
        )
        .await;
        if let Some(val) = local_cache.get(&subscriber_id) {
            cached_checkpoint = Some(*val);
        }
    }

    // A ReplayAlways subscriber must never leave a pending checkpoint behind: the
    // listener's periodic/shutdown flushes would otherwise persist it to the
    // checkpoints table, defeating replay-from-zero.
    if replay_always {
        pending_checkpoint = None;
        cached_checkpoint = None;
    }

    SubscriberBatchOutcome {
        subscriber_id,
        state,
        pending_checkpoint,
        last_event_id,
        cached_checkpoint,
        processed_any,
    }
}

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

/// A recorded gap-timeout: a global sequence a subscriber's checkpoint advanced past
/// because the gap did not fill within [`ReliableDeliveryConfig::gap_timeout`].
///
/// These records are stored in `epoch_event_bus_gap_timeouts` and can be queried
/// via [`PgEventBus::list_gap_timeouts`]. If the skipped sequence later turns out
/// to have committed (use
/// `SELECT g.* FROM epoch_event_bus_gap_timeouts g
///  JOIN epoch_events e ON e.global_sequence = g.skipped_sequence`
/// to detect this), the operator can replay the event and then call
/// [`PgEventBus::resolve_gap_timeout`] to mark the record as resolved.
#[derive(Debug, Clone)]
pub struct GapTimeoutEntry {
    /// Unique identifier for the record.
    pub id: Uuid,
    /// The bus (events table) on which the gap occurred.
    pub bus_name: String,
    /// The subscriber whose checkpoint advanced past the gap.
    pub subscriber_id: String,
    /// The `global_sequence` that was skipped.
    pub skipped_sequence: u64,
    /// How long the gap was observed before the timeout fired, in milliseconds.
    pub gap_duration_ms: i64,
    /// When the skip was recorded.
    pub timed_out_at: chrono::DateTime<chrono::Utc>,
    /// When the record was manually resolved (None if unresolved).
    pub resolved_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Identifier of the operator/system that resolved the record.
    pub resolved_by: Option<String>,
    /// Free-form notes about the resolution.
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
    /// A subscriber returned an error while processing an event in
    /// `DispatchMode::Inline`. The inner error is whatever the observer
    /// returned from `on_event`.
    #[error("Inline subscriber dispatch failed: {0}")]
    InlineDispatchError(Box<dyn std::error::Error + Send + Sync>),
    /// The requested subscriber id is not registered on this bus.
    /// Returned by readiness methods when the caller passes an id that has
    /// not been registered via [`PgEventBus::subscribe`]. Silently falling back
    /// to a checkpoint read instead would report a false-ready for a
    /// [`SubscriptionMode::ReplayAlways`] subscriber: that mode has no
    /// persisted checkpoint, so the read would find none and could be
    /// misread as "already at head" rather than "not registered".
    #[error("subscriber '{0}' is not registered on this bus")]
    SubscriberNotFound(String),
    /// Readiness gating (`subscriber_lag` / `wait_until_caught_up` /
    /// `wait_until_all_caught_up`) was called on a [`DispatchMode::Inline`] bus.
    /// Inline dispatch delivers events synchronously from `publish()` and
    /// advances neither a checkpoint nor an in-memory HWM, so these methods
    /// would otherwise poll until the caller's timeout and report a subscriber
    /// as perpetually not-ready.
    #[error(
        "readiness gating is not supported on a DispatchMode::Inline bus: events are \
         dispatched synchronously from publish() and no checkpoint or HWM position is tracked"
    )]
    InlineDispatchNotSupported,
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

/// A publish deferred by [`PgEventBus::dispatch_inline`] because it targeted a
/// bus other than the one currently draining on this task. Boxed/type-erased
/// so buses over different `EventData` types can share one queue.
type PendingPublish =
    Pin<Box<dyn Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send>>;

/// Per-task context for `DispatchMode::Inline` re-entrance and cross-bus
/// deferral.
#[derive(Clone)]
struct InlineDispatchCtx {
    /// Identities (by `inline_state` `Arc` pointer) of buses currently
    /// draining somewhere up this task's call stack — a stack, one entry per
    /// nested drain. Stable across `Clone`s of the same logical bus, distinct
    /// between different buses.
    draining: Vec<usize>,
    /// Cross-bus publishes deferred until the currently-innermost draining
    /// bus's own queue is empty, then run in FIFO order before control
    /// returns to the originating command's caller. One shared queue per
    /// true top-level dispatch, reused across every nested bus in the
    /// cascade tree so the whole tree drains to quiescence together.
    pending_cross_bus: Arc<std::sync::Mutex<VecDeque<PendingPublish>>>,
}

tokio::task_local! {
    /// `None` (unset) when the task isn't inside any inline dispatch.
    ///
    /// A `publish()` call originating from inside a subscriber handler is
    /// same-bus re-entrant only when that bus is already draining on this
    /// task (present in `draining`): the event is appended to that bus's own
    /// queue without waiting, and the active drain picks it up after the
    /// current handler returns — this is what lets a saga publish an event
    /// its own aggregate handles without deadlocking on the subscriber mutex.
    ///
    /// A publish to a bus *not* in `draining` (a cross-bus cascade, e.g. a
    /// saga on bus A dispatching a command whose event lands on bus B) is
    /// deferred onto `pending_cross_bus` rather than dispatched immediately.
    /// This matters for correctness, not just ordering: in the plain
    /// (non-transactional) `Aggregate::handle()` path, `publish()` (and thus
    /// every synchronous Inline subscriber) runs *before* the originating
    /// command's own `persist_state()`. Same-bus re-entrance is already safe
    /// because the queued event is only drained after the *whole* current
    /// `handle()` call (state persist included) returns. Dispatching a
    /// cross-bus cascade immediately, by contrast, would run its subscribers
    /// *during* that window — so a saga reacting to the cascade that calls
    /// `.handle()` again on the *same* aggregate stream would read a stale or
    /// missing state. Deferral preserves the same "only after persist_state"
    /// guarantee for cross-bus cascades that same-bus re-entrance already had.
    /// Used only in `DispatchMode::Inline`.
    static INLINE_CTX: InlineDispatchCtx;
}

/// One entry in the inline dispatch queue: the event to process plus a
/// notifier the drainer signals once every subscriber has handled it.
struct InlineQueueEntry<D>
where
    D: EventData + Send + Sync,
{
    event: Arc<Event<D>>,
    done: Arc<Notify>,
}

/// Per-bus state for the inline dispatcher. Wrapped in a `Mutex` so concurrent
/// callers (from different tasks) serialize on enqueue / dequeue. The actual
/// subscriber invocations happen outside this mutex so handlers can re-enter
/// `publish` (which only touches the mutex briefly to append).
struct InlineDispatchState<D>
where
    D: EventData + Send + Sync,
{
    queue: VecDeque<InlineQueueEntry<D>>,
    in_progress: bool,
}

impl<D> Default for InlineDispatchState<D>
where
    D: EventData + Send + Sync,
{
    fn default() -> Self {
        Self {
            queue: VecDeque::new(),
            in_progress: false,
        }
    }
}

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
    /// Inline-dispatch queue. Unused in `DispatchMode::Async`.
    inline_state: Arc<Mutex<InlineDispatchState<D>>>,
    /// Per-subscriber in-memory high-water mark for [`SubscriptionMode::ReplayAlways`]
    /// subscribers. Never persisted: a crash loses it and the next boot replays
    /// from 0, which is the intended contract. Shared across `Clone`s so
    /// `subscribe`, the listener task, and readiness queries observe the same value.
    hwm: Arc<Mutex<HashMap<String, u64>>>,
    /// `subscriber_id` -> [`SubscriptionMode`] for every registered subscriber.
    ///
    /// Readiness queries need only this static metadata, and reading it from the
    /// observers themselves would deadlock: `process_event_with_retry` holds an
    /// observer's mutex across its `on_event` await, so a subscriber that is slow
    /// or blocked makes any caller that locks it wait for the whole handler,
    /// indefinitely, ignoring the timeout it was given. This registry is only ever
    /// locked for the length of a map operation.
    subscriber_modes: Arc<Mutex<HashMap<String, SubscriptionMode>>>,
}

/// Poll cadence for `wait_until_caught_up` / `wait_until_all_caught_up`.
/// Shorter than the 1 s `flush_interval` so the gate resolves as soon as
/// processing completes rather than on the next timer tick.
pub(crate) const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(25);

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
            inline_state: Arc::new(Mutex::new(InlineDispatchState::default())),
            hwm: Arc::new(Mutex::new(HashMap::new())),
            subscriber_modes: Arc::new(Mutex::new(HashMap::new())),
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

    /// Returns the LISTEN/NOTIFY channel name this bus is bound to.
    pub fn channel_name(&self) -> &str {
        &self.channel_name
    }

    /// Returns the name of the PostgreSQL table this bus reads events from.
    pub fn events_table(&self) -> &str {
        &self.config.events_table
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
        let query = format!(
            "SELECT id, stream_id, stream_version, event_type, data, created_at, \
             actor_id, purger_id, purged_at, global_sequence, causation_id, correlation_id, schema_version \
             FROM {} WHERE global_sequence > $1 ORDER BY global_sequence ASC LIMIT $2",
            self.config.events_table,
        );
        let rows: Vec<PgDBEvent> = sqlx::query_as(&query)
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
                // CLOUD-173: carry the stored schema version through the bus read path.
                // NULL (pre-migration rows) is interpreted as version 1.
                schema_version: row.schema_version.unwrap_or(1).max(0) as u32,
            });
        }

        Ok(events)
    }

    /// Sets up the channel-specific trigger for event notifications.
    ///
    /// Creates the trigger that sends NOTIFY messages on this event bus's
    /// channel when events are inserted, if it does not already exist, and
    /// removes the pre-per-channel fixed-name trigger left behind by an earlier
    /// version, if present.
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
    /// Idempotent, but not by dropping and recreating: re-running this is a
    /// no-op once this bus's trigger exists, rather than a drop-and-recreate,
    /// which would take an ACCESS EXCLUSIVE lock on the events table on every
    /// call for no benefit.
    pub async fn setup_trigger(&self) -> Result<(), SqlxError> {
        // CLOUD-180: ensure the `txid` column exists on a custom events table so
        // snapshot-fencing forensics work. The default `epoch_events` table is
        // covered by migration m011, so it is skipped here. Failure degrades to
        // timeout-only fencing for this table (G-5), so we only warn.
        if self.config.events_table != "epoch_events"
            && let Err(e) = ensure_txid_column(&self.pool, &self.config.events_table).await
        {
            warn!(
                "Failed to ensure txid column on custom events table '{}'; \
                 snapshot fencing degrades to timeout-only for this bus: {}",
                self.config.events_table, e
            );
        }

        // CLOUD-173: ensure the `schema_version` column exists on a custom events
        // table so upcasting version metadata is persisted correctly. The default
        // `epoch_events` table is covered by migration m012, so it is skipped here.
        // Failure is non-fatal: the read path falls back to NULL → 1.
        if self.config.events_table != "epoch_events"
            && let Err(e) =
                ensure_schema_version_column(&self.pool, &self.config.events_table).await
        {
            warn!(
                "Failed to ensure schema_version column on custom events table '{}'; \
                 schema version will be read as NULL (treated as v1) for this bus: {}",
                self.config.events_table, e
            );
        }

        // Explicit, caller-invoked path: also clean up the pre-per-channel trigger
        // if an earlier version left one behind. Only done here, never in
        // start_listener: this call already hard-fails on DDL errors and is under
        // the caller's control, whereas dropping a trigger implicitly on every boot
        // is not free even when there is nothing to drop: `DROP TRIGGER IF EXISTS`
        // resolves the trigger by first taking an ACCESS EXCLUSIVE lock on the
        // relation, and `IF EXISTS` only suppresses the error once that lock is
        // held, not the lock itself. Probe with a catalog read first, so the lock
        // is paid at most once per database rather than once per boot per bus, and
        // only when there is actually a legacy trigger to remove.
        // A probe failure must not be fatal. This whole block is optional cleanup,
        // and the drop it guards was deliberately warn-only; propagating a
        // transient catalog-read error here would skip `ensure_trigger()` below and
        // leave the bus with no trigger of its own, which is far worse than failing
        // to remove a legacy one. Treat an unreadable catalog as "nothing to drop".
        let legacy_present =
            trigger_exists(&self.pool, &self.config.events_table, LEGACY_NOTIFY_TRIGGER)
                .await
                .unwrap_or_else(|e| {
                    warn!(
                        "setup_trigger: could not check for the legacy '{}' trigger on '{}'; \
                 skipping that cleanup and continuing: {}",
                        LEGACY_NOTIFY_TRIGGER, self.config.events_table, e
                    );
                    false
                });

        if legacy_present
            && let Err(e) = sqlx::query(&format!(
                "DROP TRIGGER IF EXISTS {LEGACY_NOTIFY_TRIGGER} ON {};",
                self.config.events_table,
            ))
            .execute(&self.pool)
            .await
        {
            warn!(
                "setup_trigger: failed to drop the legacy '{}' trigger on '{}' \
                 (harmless: it only produces a duplicate NOTIFY, which the \
                 per-event checkpoint check discards): {}",
                LEGACY_NOTIFY_TRIGGER, self.config.events_table, e
            );
        }

        self.ensure_trigger().await
    }

    /// Creates this bus's per-channel NOTIFY trigger on its events table if it is
    /// not already present.
    ///
    /// Shared by [`setup_trigger`](Self::setup_trigger) and the implicit trigger
    /// creation folded into [`start_listener`](Self::start_listener) (R4), so an
    /// Async bus started without an explicit `setup_trigger` still has a working
    /// trigger.
    ///
    /// Purely additive: it never drops anything. The trigger name encodes the
    /// channel (see [`notify_trigger_name`]), so this bus cannot disturb another
    /// bus's trigger on the same table, and re-running it is a no-op rather than a
    /// drop-and-recreate that would take an ACCESS EXCLUSIVE lock on every boot.
    async fn ensure_trigger(&self) -> Result<(), SqlxError> {
        let trigger_name = notify_trigger_name(&self.pool, &self.channel_name).await?;

        if trigger_exists(&self.pool, &self.config.events_table, &trigger_name).await? {
            return Ok(());
        }

        // The trigger name is prefix + hex from md5, so it needs no quoting. The
        // channel is a function argument, with single quotes escaped.
        let result = sqlx::query(&format!(
            "CREATE TRIGGER {} \
             AFTER INSERT ON {} \
             FOR EACH ROW \
             EXECUTE FUNCTION epoch_notify_event('{}');",
            trigger_name,
            self.config.events_table,
            self.channel_name.replace('\'', "''")
        ))
        .execute(&self.pool)
        .await;

        // The exists-check above is not atomic with this CREATE: two replicas
        // booting on the same channel at once can both see `false`. The loser
        // gets SQLSTATE 42710 (duplicate_object), which is semantically success
        // here — the trigger this call wanted now exists — so treat it as such
        // rather than failing a boot on a race that resolved in our favour.
        const DUPLICATE_OBJECT: &str = "42710";
        if let Err(e) = &result {
            let is_duplicate = e
                .as_database_error()
                .and_then(|db_err| db_err.code())
                .is_some_and(|code| code == DUPLICATE_OBJECT);
            if is_duplicate {
                return Ok(());
            }
        }
        result?;

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
        // In inline-dispatch mode there is no background listener: events are
        // delivered to subscribers synchronously from `publish()`. Make this a
        // no-op so existing call sites (production initialization, tests) work
        // unchanged when they swap the dispatch mode.
        if self.config.dispatch_mode == config::DispatchMode::Inline {
            log::debug!(
                "start_listener called on inline-dispatch bus: nothing to start, dispatch happens in publish()"
            );
            return Ok(());
        }

        // Check if already started
        {
            let state = self.listener_state.lock().await;
            if state.is_some() {
                warn!("Listener already started, ignoring duplicate start_listener call");
                return Ok(());
            }
        }

        // R4: fold trigger creation into start_listener so an Async bus is never
        // left relying on the timer tick because setup_trigger was never called.
        // `ensure_trigger` is additive and creates only when this bus's own
        // per-channel trigger is missing, so this neither disturbs another bus on
        // the same table nor takes an ACCESS EXCLUSIVE lock on an ordinary boot.
        //
        // Never hard-fail: an app role without DDL rights (trigger created
        // out-of-band by a migration/DBA) must still be able to start a listener —
        // the periodic timer tick plus the R2 catch-up pass below keep delivery
        // correct without it.
        if let Err(e) = self.ensure_trigger().await {
            warn!(
                "start_listener: failed to ensure the NOTIFY trigger on '{}' \
                 (continuing without it; delivery falls back to the periodic \
                 timer tick and the startup catch-up pass): {}",
                self.config.events_table, e
            );
        }

        let listener_pool = self.pool.clone();
        let checkpoint_pool = self.pool.clone();
        let dlq_pool = self.pool.clone();
        let channel_name = self.channel_name.clone();
        let projections = self.projections.clone();
        let config = self.config.clone();
        let hwm = self.hwm.clone();

        // Create shutdown signal channel
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

        let handle = tokio::spawn(async move {
            // No priority sort of `projections` here. The batch loop below reads
            // each subscriber's priority per wake and dispatches in ascending
            // priority order, and inline dispatch sorts independently, so the
            // stored order carries no meaning. Sorting it once at startup was
            // both redundant and unsafe: reading priorities requires locking each
            // observer, and `process_event_with_retry` holds an observer's mutex
            // across its `on_event` await, so doing that under the `projections`
            // guard let one slow handler block every `subscribe()`; doing it off
            // the guard and writing the result back instead dropped any
            // subscriber that registered while the sort was in flight.

            // R2: run one checkpoint-driven catch-up pass over every registered
            // subscriber before entering the select loop, so a readiness gate
            // resolves without waiting for the first NOTIFY or the flush_interval
            // tick. Transient DB errors are logged and retried on the next loop
            // iteration; the listener still starts (at-least-once discipline,
            // matching subscribe()).
            //
            // Snapshot the subscriber list and release the projections lock before
            // awaiting the (potentially long) per-subscriber replay: holding the
            // lock across it would block every other caller of subscribe_lag /
            // wait_until_caught_up / wait_until_all_caught_up (which also lock
            // projections) for the full duration of this pass, defeating their
            // bounded `timeout`, and risks a deadlock if an observer's own
            // `on_event` re-enters the bus. Mirrors fast_forward_all_subscribers.
            let projections_snapshot: Vec<_> = {
                let guard = projections.lock().await;
                guard.iter().cloned().collect()
            };

            // Replay in priority order, so a saga (priority 100) does not catch up
            // against a read model (priority 0) that has not yet seen the same
            // events. Each replay below is awaited sequentially, so this ordering is
            // the only thing establishing it. Sorted here, on the snapshot and off
            // the `projections` guard, rather than by reordering the shared vector:
            // reading a priority means locking an observer, and an observer's mutex
            // is held across its `on_event`.
            let mut tagged = Vec::with_capacity(projections_snapshot.len());
            for projection in &projections_snapshot {
                let (priority, subscriber_id, replay_always, failure_mode) = {
                    let obs = projection.lock().await;
                    (
                        obs.priority(),
                        obs.subscriber_id().to_string(),
                        obs.subscription_mode() == SubscriptionMode::ReplayAlways,
                        obs.failure_mode(),
                    )
                };
                tagged.push((
                    priority,
                    subscriber_id,
                    replay_always,
                    failure_mode,
                    projection,
                ));
            }
            tagged.sort_by_key(|(priority, _, _, _, _)| *priority);

            for (_, subscriber_id, replay_always, failure_mode, projection) in tagged {
                if let Err(e) = catch_up_from_checkpoint(
                    projection,
                    &subscriber_id,
                    replay_always,
                    failure_mode,
                    &config,
                    &checkpoint_pool,
                    &hwm,
                )
                .await
                {
                    warn!(
                        "start_listener: initial catch-up for '{}' failed: {}; \
                         the listener will retry on its next loop iteration",
                        subscriber_id, e
                    );
                }
            }

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
                                &config.events_table,
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
                                        listener_option.insert(l)
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
                            &config.events_table,
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
                                &config.events_table,
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
                //
                // SHARED FETCH: Events are fetched ONCE per batch from the minimum
                // checkpoint across all subscribers. Each subscriber then processes
                // only events above its own checkpoint. This ensures all subscribers
                // see the same snapshot of events, preventing race conditions where
                // a projection misses events that a later saga sees.

                // Snapshot the subscriber list and release the lock before doing any
                // work: every readiness API (`subscriber_lag`,
                // `wait_until_caught_up`, `wait_until_all_caught_up`) also locks
                // `projections`, so holding the guard across the drain below made them
                // block for its full duration and silently overrun their own timeout
                // (CLOUD-225). Mirrors the R2 catch-up pass above.
                //
                // Releasing it lets a `subscribe()` land mid-drain. That is safe and
                // does not double-deliver: `subscribe()` registers its observer last,
                // only after its own synchronous catch-up and checkpoint flush, and the
                // per-event checkpoint check skips anything at or below the checkpoint.
                // A subscriber that arrives mid-drain is simply picked up on the next
                // wake.
                let projections_snapshot: Vec<_> = {
                    let guard = projections.lock().await;
                    guard.iter().cloned().collect()
                };

                // Initialize per-subscriber state for any new subscribers.
                for projection in projections_snapshot.iter() {
                    let (subscriber_id, replay_always, failure_mode) = {
                        let guard = projection.lock().await;
                        (
                            guard.subscriber_id().to_string(),
                            guard.subscription_mode() == SubscriptionMode::ReplayAlways,
                            guard.failure_mode(),
                        )
                    };
                    if !subscriber_states.contains_key(&subscriber_id) {
                        // R5 (Correction 2): a ReplayAlways subscriber has no
                        // checkpoint row, so seeding from the checkpoint table would
                        // give 0 and re-deliver the entire history on the first live
                        // batch. Seed from the in-memory HWM (set at the end of
                        // subscribe()'s / the R2 pass's catch-up) instead.
                        // Seed both the contiguous position and its paired
                        // `event_id` (spec 0027 §4 Q1) so an eager
                        // `PendingCheckpoint` keeps `last_event_id` paired with
                        // `last_global_sequence` (spec 0026 R4). A NULL stored id
                        // maps to nil, which is unpublishable by construction and
                        // so is never re-written.
                        let (checkpoint, checkpoint_event_id) = if replay_always {
                            (
                                hwm.lock().await.get(&subscriber_id).copied().unwrap_or(0),
                                Uuid::nil(),
                            )
                        } else {
                            match sqlx::query_as::<_, (i64, Option<Uuid>)>(
                                r#"
                                SELECT last_global_sequence, last_event_id
                                FROM epoch_event_bus_checkpoints
                                WHERE bus_name = $1 AND subscriber_id = $2
                                "#,
                            )
                            .bind(&config.events_table)
                            .bind(&subscriber_id)
                            .fetch_optional(&checkpoint_pool)
                            .await
                            {
                                Ok(Some((seq, id))) => (seq as u64, id.unwrap_or_else(Uuid::nil)),
                                Ok(None) => (0, Uuid::nil()),
                                Err(e) => {
                                    warn!(
                                        "Failed to load checkpoint for '{}': {}, starting from 0",
                                        subscriber_id, e
                                    );
                                    (0, Uuid::nil())
                                }
                            }
                        };
                        subscriber_states.insert(
                            subscriber_id.clone(),
                            SubscriberState::new_with_event_id(
                                checkpoint,
                                checkpoint_event_id,
                                failure_mode,
                            ),
                        );
                    }
                }

                // Shared batch loop: fetch events once from the minimum checkpoint,
                // then fan out to all subscribers in priority order.
                loop {
                    // Cooperative shutdown check. This loop can run for the whole
                    // backlog and only the outer `select!` observes
                    // `shutdown_rx.changed()` between wakes, so without this a
                    // `shutdown()` caller would wait unbounded for a drain that
                    // never looks at the signal. Checked at the top of the loop,
                    // after the previous batch's outcomes are already merged back
                    // into `subscriber_states`/`pending_checkpoints` (never
                    // mid-batch), so breaking here abandons nothing: already-
                    // flushed checkpoints stay correct and the outer loop's
                    // shutdown branch still runs the final flush. `borrow()`
                    // rather than `changed()`: it must not consume the pending
                    // change, or the outer `select!`'s own `shutdown_rx.changed()`
                    // arm would never fire and the listener would spin on the
                    // timer tick forever instead of shutting down.
                    if *shutdown_rx.borrow() {
                        info!(
                            "Shutdown signal received mid-drain; stopping the batch \
                             loop at the next batch boundary"
                        );
                        break;
                    }

                    // Find the minimum contiguous checkpoint across all subscribers.
                    let min_checkpoint = subscriber_states
                        .values()
                        .map(|s| s.contiguous_checkpoint)
                        .min()
                        .unwrap_or(0);

                    let catchup_query = format!(
                        "SELECT id, stream_id, stream_version, event_type, data, \
                         created_at, actor_id, purger_id, purged_at, \
                         global_sequence, causation_id, correlation_id, schema_version \
                         FROM {} WHERE global_sequence > $1 \
                         ORDER BY global_sequence ASC LIMIT $2",
                        config.events_table,
                    );
                    let rows: Vec<PgDBEvent> = match sqlx::query_as(&catchup_query)
                        .bind(min_checkpoint as i64)
                        .bind(config.catch_up_batch_size as i64)
                        .fetch_all(&checkpoint_pool)
                        .await
                    {
                        Ok(rows) => rows,
                        Err(e) => {
                            error!("Failed to query events: {}", e);
                            break;
                        }
                    };

                    if rows.is_empty() {
                        break;
                    }

                    let batch_was_full = rows.len() == config.catch_up_batch_size as usize;

                    // Pre-compute shared indexes from the batch.
                    let visible_seqs: BTreeSet<u64> = rows
                        .iter()
                        .filter_map(|r| r.global_sequence.map(|gs| gs as u64))
                        .collect();

                    let seq_to_id: HashMap<u64, Uuid> = rows
                        .iter()
                        .filter_map(|r| r.global_sequence.map(|gs| (gs as u64, r.id)))
                        .collect();

                    let mut any_subscriber_processed = false;

                    // Process the shared batch with concurrent dispatch within each
                    // priority group. Projections (priority 0) must complete before
                    // sagas (priority 100) so read models are up-to-date when sagas
                    // query them. Within a priority group, subscribers run concurrently.
                    let shared_rows = Arc::new(rows);
                    let shared_visible = Arc::new(visible_seqs);
                    let shared_seq_to_id = Arc::new(seq_to_id);

                    // CLOUD-180: acquire the transaction-id snapshot once per batch,
                    // but only when fencing is enabled and at least one subscriber
                    // already has an active gap (OQ-4). Brand-new gaps detected in
                    // this batch get a `fence_xmax = None` observation and are
                    // backfilled on the next batch (which will now see an active
                    // gap and query the snapshot). This keeps the common no-gap
                    // path at zero extra round trips.
                    let batch_snapshot = if config.snapshot_fencing
                        && subscriber_states
                            .values()
                            .any(|s| !s.gap_first_seen.is_empty())
                    {
                        query_txid_snapshot(&checkpoint_pool).await
                    } else {
                        None
                    };

                    // Collect (priority, subscriber_id, projection) tuples sequentially,
                    // acquiring each per-subscriber lock one at a time to read metadata.
                    let mut tagged = Vec::new();
                    for projection in projections_snapshot.iter() {
                        let guard = projection.lock().await;
                        let priority = guard.priority();
                        let sid = guard.subscriber_id().to_string();
                        drop(guard);
                        tagged.push((priority, sid, projection.clone()));
                    }

                    // Find distinct priorities in ascending order.
                    let mut priorities: Vec<u8> = tagged.iter().map(|(p, _, _)| *p).collect();
                    priorities.sort_unstable();
                    priorities.dedup();

                    for priority in priorities {
                        // Build task inputs, deduplicating by subscriber_id.
                        // If the same subscriber_id appears multiple times in projections
                        // (e.g. registered by both core and web), only the first wins.
                        // Without deduplication, the second occurrence would get
                        // SubscriberState::new(0) from the remove fallback and reprocess
                        // all historical events.
                        let mut seen_sids = std::collections::HashSet::<String>::new();
                        let task_inputs: Vec<_> = tagged
                            .iter()
                            .filter(|(p, sid, _)| *p == priority && seen_sids.insert(sid.clone()))
                            .map(|(_, sid, proj)| {
                                let state = subscriber_states
                                    .remove(sid)
                                    .unwrap_or_else(|| SubscriberState::new(0));
                                let pending = pending_checkpoints.remove(sid);
                                let last_id = last_event_ids.get(sid).copied();
                                (
                                    sid.clone(),
                                    proj.clone(),
                                    state,
                                    pending,
                                    last_id,
                                    BatchContext {
                                        rows: shared_rows.clone(),
                                        visible_seqs: shared_visible.clone(),
                                        seq_to_id: shared_seq_to_id.clone(),
                                        config: config.clone(),
                                        dlq_pool: dlq_pool.clone(),
                                        checkpoint_pool: checkpoint_pool.clone(),
                                        snapshot: batch_snapshot,
                                        hwm: hwm.clone(),
                                    },
                                )
                            })
                            .collect();

                        let outcomes = join_all(task_inputs.into_iter().map(
                            |(sid, proj, state, pending, last_id, batch_ctx)| {
                                process_subscriber_for_batch(
                                    proj, sid, state, pending, last_id, batch_ctx,
                                )
                            },
                        ))
                        .await;

                        // Merge outcomes back into the state maps.
                        for outcome in outcomes {
                            if outcome.processed_any {
                                any_subscriber_processed = true;
                            }
                            if let Some(val) = outcome.cached_checkpoint {
                                checkpoint_cache.insert(outcome.subscriber_id.clone(), val);
                            }
                            if let Some(id) = outcome.last_event_id {
                                last_event_ids.insert(outcome.subscriber_id.clone(), id);
                            }
                            if let Some(p) = outcome.pending_checkpoint {
                                pending_checkpoints.insert(outcome.subscriber_id.clone(), p);
                            }
                            subscriber_states.insert(outcome.subscriber_id, outcome.state);
                        }
                    }

                    if !batch_was_full || !any_subscriber_processed {
                        break;
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
            WHERE bus_name = $1 AND subscriber_id = $2
            "#,
        )
        .bind(&self.config.events_table)
        .bind(subscriber_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(result.map(|(seq,)| seq as u64))
    }

    /// Updates or creates a checkpoint for a subscriber.
    ///
    /// This is an upsert operation - it will create a new checkpoint if one doesn't exist,
    /// or update the existing one.
    ///
    /// # Hazard: this is a blind, non-monotonic write
    /// Like this bus's internal flush primitive, the underlying `DO UPDATE SET
    /// last_global_sequence = EXCLUDED...` has no guard against moving the
    /// checkpoint backwards. Pass only a `global_sequence` this subscriber has
    /// actually finished processing up to a contiguous prefix (spec 0026 R1);
    /// passing anything else can strand events below the seed of a subsequent
    /// listener restart the same way the bug this crate fixes did.
    pub async fn update_checkpoint(
        &self,
        subscriber_id: &str,
        global_sequence: u64,
        event_id: Uuid,
    ) -> Result<(), SqlxError> {
        sqlx::query(
            r#"
            INSERT INTO epoch_event_bus_checkpoints (bus_name, subscriber_id, last_global_sequence, last_event_id, updated_at)
            VALUES ($1, $2, $3, $4, NOW())
            ON CONFLICT (bus_name, subscriber_id) DO UPDATE SET
                last_global_sequence = EXCLUDED.last_global_sequence,
                last_event_id = EXCLUDED.last_event_id,
                updated_at = NOW()
            "#,
        )
        .bind(&self.config.events_table)
        .bind(subscriber_id)
        .bind(global_sequence as i64)
        .bind(event_id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    // -------------------------------------------------------------------------
    // spec 0024 R1 — Subscriber lag / readiness
    // -------------------------------------------------------------------------

    /// The highest `global_sequence` currently present on this bus's events
    /// table, or `None` if the table is empty.
    ///
    /// Note: with non-transactional `nextval` (spec 0019) the head may be a
    /// burned/in-flight value that never becomes visible, because the
    /// transaction that reserved it rolled back or is still open. Readiness
    /// does not wait on such a value directly: the listener's gap-timeout
    /// mechanism advances a subscriber's contiguous checkpoint past a sequence
    /// like this once it has been missing long enough to conclude it never
    /// will become visible.
    pub async fn head_sequence(&self) -> Result<Option<u64>, PgEventBusError> {
        // `SELECT MAX(...)` always returns exactly one row (NULL when the
        // table is empty), so use fetch_one with Option<i64>.
        let row: (Option<i64>,) = sqlx::query_as(&format!(
            "SELECT MAX(global_sequence) FROM {}",
            self.config.events_table,
        ))
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0.map(|v| v as u64))
    }

    /// Returns `Err(InlineDispatchNotSupported)` if this bus is
    /// `DispatchMode::Inline`. Shared entry check for every readiness method:
    /// Inline dispatch advances neither a checkpoint nor the in-memory HWM, so
    /// polling position would silently burn the caller's whole timeout and
    /// always report "not ready" rather than surfacing a clear error.
    fn require_non_inline_dispatch(&self) -> Result<(), PgEventBusError> {
        if self.config.dispatch_mode == config::DispatchMode::Inline {
            return Err(PgEventBusError::InlineDispatchNotSupported);
        }
        Ok(())
    }

    /// Returns the `SubscriptionMode` for `subscriber_id`, or
    /// `SubscriberNotFound` if it is not registered.
    async fn subscriber_mode(
        &self,
        subscriber_id: &str,
    ) -> Result<SubscriptionMode, PgEventBusError> {
        // Read the registry rather than the observers: locking an observer here
        // would block for the duration of its current `on_event`, which is
        // unbounded (see `subscriber_modes`).
        self.subscriber_modes
            .lock()
            .await
            .get(subscriber_id)
            .copied()
            .ok_or_else(|| PgEventBusError::SubscriberNotFound(subscriber_id.to_string()))
    }

    /// Reads the current position for `subscriber_id` given its already-resolved
    /// `mode`, without touching the projections registry.
    async fn position_for_mode(
        &self,
        subscriber_id: &str,
        mode: SubscriptionMode,
    ) -> Result<u64, PgEventBusError> {
        match mode {
            SubscriptionMode::Checkpointed => {
                Ok(self.get_checkpoint(subscriber_id).await?.unwrap_or(0))
            }
            SubscriptionMode::ReplayAlways => Ok(self
                .hwm
                .lock()
                .await
                .get(subscriber_id)
                .copied()
                .unwrap_or(0)),
            // `SubscriptionMode` is `#[non_exhaustive]`, so a future variant added
            // by epoch_core lands here. Silently falling back to the Checkpointed
            // read would risk the exact false-ready this dispatch exists to avoid
            // (Correction 4) for a mode whose position isn't tracked by a
            // checkpoint at all. Report unconditionally not-ready instead.
            other => {
                warn!(
                    "position_for_mode: unrecognized SubscriptionMode {other:?} for '{}'; \
                     treating as position 0 (never ready) rather than guessing",
                    subscriber_id
                );
                Ok(0)
            }
        }
    }

    /// Resolves the current position of `subscriber_id`.
    ///
    /// - `Checkpointed` → DB checkpoint (0 if no row yet).
    /// - `ReplayAlways` → in-memory HWM (0 if not yet seeded).
    /// - Not registered → `Err(PgEventBusError::SubscriberNotFound)`.
    async fn subscriber_position(&self, subscriber_id: &str) -> Result<u64, PgEventBusError> {
        let mode = self.subscriber_mode(subscriber_id).await?;
        self.position_for_mode(subscriber_id, mode).await
    }

    /// Events this subscriber is behind the current head, for monitoring.
    ///
    /// `head − position`, saturating at 0. `position` is the highest
    /// *contiguous* prefix persisted for a [`SubscriptionMode::Checkpointed`]
    /// subscriber, or the in-memory high-water mark for a
    /// [`SubscriptionMode::ReplayAlways`] one. Point-in-time; the head may move
    /// under a cascade.
    ///
    /// Returns [`PgEventBusError::SubscriberNotFound`] if `subscriber_id` is
    /// not registered on this bus, or [`PgEventBusError::InlineDispatchNotSupported`]
    /// if this bus is [`DispatchMode::Inline`](crate::DispatchMode::Inline) (Inline
    /// dispatch tracks no checkpoint or HWM position to measure lag against).
    ///
    /// # Hazard: one slow subscriber can hold up the rest
    /// Subscribers in the same priority group are processed together, and the
    /// batch loop cannot fetch the next batch until every subscriber in the
    /// group has finished the current one. A slow or wedged `on_event` on a
    /// peer therefore holds this subscriber's own lag non-zero even while its
    /// own handler is healthy and fast.
    ///
    /// # Hazard: readiness is measured on the contiguous checkpoint
    /// `position` is the highest *contiguous* prefix processed, not the
    /// highest sequence seen (spec 0026 R1). A hole ANYWHERE in the backlog —
    /// not only at the tail, which is all the burned/in-flight hazard above
    /// covers — pins `position` at the hole's location until it resolves, so
    /// lag can stay non-zero even though every event visible so far has
    /// actually been processed. See spec 0026 R5.
    pub async fn subscriber_lag(&self, subscriber_id: &str) -> Result<u64, PgEventBusError> {
        self.require_non_inline_dispatch()?;
        // Validate subscriber first so unknown ids get SubscriberNotFound
        // even when the bus has no events (Correction 4).
        let pos = self.subscriber_position(subscriber_id).await?;
        let head = self.head_sequence().await?.unwrap_or(0);
        Ok(head.saturating_sub(pos))
    }

    /// Shared polling loop for [`wait_until_caught_up`](Self::wait_until_caught_up)
    /// and [`wait_until_all_caught_up`](Self::wait_until_all_caught_up): polls every
    /// entry's position against `target` at [`READINESS_POLL_INTERVAL`] until all
    /// are `>= target` or `deadline` elapses.
    async fn poll_until_all_at_or_past(
        &self,
        subscribers: &[(String, SubscriptionMode)],
        target: u64,
        deadline: tokio::time::Instant,
    ) -> Result<bool, PgEventBusError> {
        loop {
            let mut all_caught_up = true;
            for (id, mode) in subscribers {
                let pos = self.position_for_mode(id, *mode).await?;
                if pos < target {
                    all_caught_up = false;
                    break;
                }
            }
            if all_caught_up {
                return Ok(true);
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(false);
            }
            sleep(READINESS_POLL_INTERVAL).await;
        }
    }

    /// Awaits until `subscriber_id` has processed every event up to the head
    /// observed **at call time** (`target = head_sequence()`), or `timeout`
    /// elapses.
    ///
    /// Snapshots `target` once and polls `position >= target`; it deliberately
    /// does NOT chase a head that grows during the wait (cross-bus convergence
    /// is spec 0025's concern). Returns `Ok(true)` if caught up, `Ok(false)`
    /// on timeout.
    ///
    /// # Hazard: burned/in-flight tail
    /// `target` is `head_sequence()`, which can be a burned or in-flight
    /// `nextval` (spec 0019) that only ever becomes visible via the gap
    /// backstop. When the tail is such a sequence, `position` cannot reach
    /// `target` until the gap timeout elapses, so callers must pass a
    /// `timeout` larger than the subscriber's `gap_timeout` to avoid a
    /// spurious `Ok(false)`.
    ///
    /// # Hazard: this is a LOCAL readiness check
    /// This gates only *this* subscriber against *this* bus's head at call
    /// time. It is **unsafe as a startup gate for any consumer whose sagas
    /// cascade across buses**: a subscriber can be "caught up" to the current
    /// head while an upstream saga on another bus is still about to extend it.
    /// Cross-bus readiness needs the fixed-point gate in **spec 0025**.
    ///
    /// # Hazard: one slow subscriber can hold up the rest
    /// Subscribers in the same priority group are processed together, and the
    /// batch loop cannot start fetching the next batch until every subscriber
    /// in the group has finished the current one. A single slow or wedged
    /// `on_event` on a peer therefore stalls this subscriber's progress too,
    /// for the rest of the backlog, even though this subscriber's own handler
    /// is healthy.
    ///
    /// # Hazard: readiness is measured on the contiguous checkpoint
    /// `position` is the highest *contiguous* prefix processed, not the
    /// highest sequence seen (spec 0026 R1). A hole ANYWHERE in the backlog —
    /// not only at the tail, which is all the burned/in-flight hazard above
    /// covers — pins `position` at the hole's location until it resolves, so
    /// this call can stay pending even though every event visible so far has
    /// actually been processed. See spec 0026 R5.
    ///
    /// Returns [`PgEventBusError::SubscriberNotFound`] if `subscriber_id` is
    /// not registered on this bus, or [`PgEventBusError::InlineDispatchNotSupported`]
    /// if this bus is [`DispatchMode::Inline`](crate::DispatchMode::Inline).
    pub async fn wait_until_caught_up(
        &self,
        subscriber_id: &str,
        timeout: Duration,
    ) -> Result<bool, PgEventBusError> {
        self.require_non_inline_dispatch()?;
        // Validate subscriber and resolve mode once (Correction 4: fail with
        // SubscriberNotFound even when the bus is empty, before reading
        // head_sequence). Mode can't change during the wait, so resolve once.
        let mode = self.subscriber_mode(subscriber_id).await?;

        let target = match self.head_sequence().await? {
            Some(h) => h,
            None => return Ok(true), // empty bus — trivially caught up
        };

        let deadline = tokio::time::Instant::now() + timeout;
        self.poll_until_all_at_or_past(&[(subscriber_id.to_string(), mode)], target, deadline)
            .await
    }

    /// Bus-wide variant: [`wait_until_caught_up`][Self::wait_until_caught_up]
    /// for every registered subscriber against a single head snapshot taken at
    /// call time, under one shared timeout.
    ///
    /// Returns `Ok(true)` only if **all** registered subscribers are caught up
    /// before the timeout, `Ok(false)` otherwise. Returns
    /// [`PgEventBusError::InlineDispatchNotSupported`] if this bus is
    /// [`DispatchMode::Inline`](crate::DispatchMode::Inline).
    ///
    /// # Hazard: an empty registry is trivially "caught up"
    /// If no subscribers are registered yet, this returns `Ok(true)`
    /// immediately rather than waiting: there is nothing to be caught up
    /// *with*. The likeliest way to hit this is calling it as a startup gate
    /// before `subscribe()` has been called for every consumer that should be
    /// covered, which reports ready before those subscribers even exist rather
    /// than before they are caught up.
    ///
    /// # Hazard: one slow subscriber can hold up the rest
    /// Subscribers in the same priority group are processed together, and the
    /// batch loop cannot start fetching the next batch until every subscriber
    /// in the group has finished the current one. A single slow or wedged
    /// `on_event` therefore stalls every other subscriber in its group for the
    /// rest of the backlog, not just its own progress, and this call cannot
    /// return `true` for any of them until it clears.
    ///
    /// # Hazard: readiness is measured on the contiguous checkpoint
    /// Each subscriber's `position` is the highest *contiguous* prefix
    /// processed, not the highest sequence seen (spec 0026 R1). A hole
    /// ANYWHERE in the backlog — not only at the tail, which is all the
    /// burned/in-flight hazard on [`wait_until_caught_up`](Self::wait_until_caught_up)
    /// covers — pins that subscriber's `position` at the hole's location until
    /// it resolves, so this call can stay pending even though every event
    /// visible so far has actually been processed. See spec 0026 R5.
    pub async fn wait_until_all_caught_up(
        &self,
        timeout: Duration,
    ) -> Result<bool, PgEventBusError> {
        self.require_non_inline_dispatch()?;
        let target = match self.head_sequence().await? {
            Some(h) => h,
            None => return Ok(true), // empty bus — trivially caught up
        };

        // Snapshot ids and modes once; mode can't change during the wait and
        // this avoids a full registry scan on every 25 ms poll iteration. Taken
        // from the registry, never from the observers, which may each be locked
        // for the length of an in-flight `on_event` (see `subscriber_modes`).
        let subscribers: Vec<(String, SubscriptionMode)> = self
            .subscriber_modes
            .lock()
            .await
            .iter()
            .map(|(id, mode)| (id.clone(), *mode))
            .collect();

        if subscribers.is_empty() {
            return Ok(true);
        }

        let deadline = tokio::time::Instant::now() + timeout;
        self.poll_until_all_at_or_past(&subscribers, target, deadline)
            .await
    }

    /// Fast-forwards every currently-registered subscriber's checkpoint to the
    /// current head of this bus (its maximum `global_sequence`).
    ///
    /// This is intended for bulk-loading / seeding scenarios that publish events
    /// with [`DispatchMode::Inline`](crate::DispatchMode): the
    /// read models are built synchronously inside each command, but no bus
    /// checkpoints are written. Without fast-forwarding, a subsequently started
    /// `Async` listener would re-process the entire history from sequence 0
    /// (re-running projections and re-firing sagas, and potentially failing on
    /// events whose referenced state has since been removed).
    ///
    /// Call this once, after all subscribers have been registered and all seed
    /// events have been published. It upserts a checkpoint at the bus head for
    /// each registered subscriber, so a later `Async` start resumes from head.
    ///
    /// No-op if the bus has no events yet.
    pub async fn fast_forward_all_subscribers(&self) -> Result<(), SqlxError> {
        // Read the current head (max global_sequence and its event id).
        let head: Option<(i64, Uuid)> = sqlx::query_as(&format!(
            "SELECT global_sequence, id FROM {} \
             ORDER BY global_sequence DESC LIMIT 1",
            self.config.events_table,
        ))
        .fetch_optional(&self.pool)
        .await?;

        let Some((max_seq, last_event_id)) = head else {
            // No events on this bus; nothing to fast-forward.
            return Ok(());
        };

        // Snapshot the observer list and release the `projections` guard before
        // locking each observer: `process_event_with_retry` holds an observer's
        // mutex across its `on_event` await, so doing this under `projections`
        // would let one slow handler block every other caller of `subscribe()`/
        // `start_listener()` for as long as that handler runs. R5: skip
        // ReplayAlways subscribers — they have no persisted checkpoint, and
        // parking one at head would suppress the replay-from-zero their next
        // boot depends on.
        let snapshot: Vec<_> = { self.projections.lock().await.iter().cloned().collect() };
        let subscriber_ids: Vec<String> = {
            let mut ids = Vec::with_capacity(snapshot.len());
            for observer in &snapshot {
                let o = observer.lock().await;
                if o.subscription_mode() == SubscriptionMode::ReplayAlways {
                    continue;
                }
                ids.push(o.subscriber_id().to_string());
            }
            ids
        };

        for subscriber_id in subscriber_ids {
            self.update_checkpoint(&subscriber_id, max_seq as u64, last_event_id)
                .await?;
        }

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

    /// Lists gap-timeout records for this bus.
    ///
    /// Each row represents a `global_sequence` that a subscriber's checkpoint was advanced
    /// past because the gap did not fill within [`ReliableDeliveryConfig::gap_timeout`]. The
    /// records are scoped to this bus's `events_table` (the `bus_name` column).
    ///
    /// # Arguments
    ///
    /// * `subscriber_id` - Restrict results to one subscriber, or `None` for all subscribers
    ///   on this bus.
    /// * `unresolved_only` - When `true`, only rows with `resolved_at IS NULL` are returned.
    /// * `offset` - Number of rows to skip (for pagination).
    /// * `limit` - Maximum number of rows to return.
    ///
    /// # Returns
    ///
    /// A vector of gap-timeout entries ordered by `timed_out_at` ascending (oldest first).
    pub async fn list_gap_timeouts(
        &self,
        subscriber_id: Option<&str>,
        unresolved_only: bool,
        offset: u64,
        limit: u64,
    ) -> Result<Vec<GapTimeoutEntry>, SqlxError> {
        // Build the query dynamically based on optional filters.
        // Using sqlx's query builder would be cleaner for truly dynamic queries;
        // here we enumerate the four combinations for clarity and type safety.
        let bus_name = &self.config.events_table;
        use sqlx::Row;

        let rows = match (subscriber_id, unresolved_only) {
            (Some(sub), true) => {
                sqlx::query(
                    r#"
                    SELECT id, bus_name, subscriber_id, skipped_sequence, gap_duration_ms,
                           timed_out_at, resolved_at, resolved_by, resolution_notes
                    FROM epoch_event_bus_gap_timeouts
                    WHERE bus_name = $1
                      AND subscriber_id = $2
                      AND resolved_at IS NULL
                    ORDER BY timed_out_at ASC
                    OFFSET $3 LIMIT $4
                    "#,
                )
                .bind(bus_name)
                .bind(sub)
                .bind(offset as i64)
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await?
            }
            (Some(sub), false) => {
                sqlx::query(
                    r#"
                    SELECT id, bus_name, subscriber_id, skipped_sequence, gap_duration_ms,
                           timed_out_at, resolved_at, resolved_by, resolution_notes
                    FROM epoch_event_bus_gap_timeouts
                    WHERE bus_name = $1
                      AND subscriber_id = $2
                    ORDER BY timed_out_at ASC
                    OFFSET $3 LIMIT $4
                    "#,
                )
                .bind(bus_name)
                .bind(sub)
                .bind(offset as i64)
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await?
            }
            (None, true) => {
                sqlx::query(
                    r#"
                    SELECT id, bus_name, subscriber_id, skipped_sequence, gap_duration_ms,
                           timed_out_at, resolved_at, resolved_by, resolution_notes
                    FROM epoch_event_bus_gap_timeouts
                    WHERE bus_name = $1
                      AND resolved_at IS NULL
                    ORDER BY timed_out_at ASC
                    OFFSET $2 LIMIT $3
                    "#,
                )
                .bind(bus_name)
                .bind(offset as i64)
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await?
            }
            (None, false) => {
                sqlx::query(
                    r#"
                    SELECT id, bus_name, subscriber_id, skipped_sequence, gap_duration_ms,
                           timed_out_at, resolved_at, resolved_by, resolution_notes
                    FROM epoch_event_bus_gap_timeouts
                    WHERE bus_name = $1
                    ORDER BY timed_out_at ASC
                    OFFSET $2 LIMIT $3
                    "#,
                )
                .bind(bus_name)
                .bind(offset as i64)
                .bind(limit as i64)
                .fetch_all(&self.pool)
                .await?
            }
        };

        Ok(rows
            .into_iter()
            .map(|row| GapTimeoutEntry {
                id: row.get("id"),
                bus_name: row.get("bus_name"),
                subscriber_id: row.get("subscriber_id"),
                skipped_sequence: row.get::<i64, _>("skipped_sequence") as u64,
                gap_duration_ms: row.get("gap_duration_ms"),
                timed_out_at: row.get("timed_out_at"),
                resolved_at: row.get("resolved_at"),
                resolved_by: row.get("resolved_by"),
                resolution_notes: row.get("resolution_notes"),
            })
            .collect())
    }

    /// Marks a gap-timeout record as resolved.
    ///
    /// Use this after confirming the skipped sequence came from a rolled-back
    /// transaction (no action needed) or after replaying the late-committed event
    /// through other means. Records the operator identity and optional notes for
    /// audit purposes.
    ///
    /// # Arguments
    ///
    /// * `id` - The UUID of the gap-timeout record to resolve.
    /// * `resolved_by` - Identifier of the operator or system performing the resolution.
    /// * `resolution_notes` - Optional free-form notes describing the resolution action.
    ///
    /// # Returns
    ///
    /// `true` if a record was updated (i.e., an unresolved record with the given `id`
    /// existed), `false` if no matching unresolved record was found (already resolved
    /// or id does not exist).
    pub async fn resolve_gap_timeout(
        &self,
        id: Uuid,
        resolved_by: &str,
        resolution_notes: Option<&str>,
    ) -> Result<bool, SqlxError> {
        let result = sqlx::query(
            r#"
            UPDATE epoch_event_bus_gap_timeouts
            SET resolved_at = NOW(),
                resolved_by = $2,
                resolution_notes = $3
            WHERE id = $1
              AND resolved_at IS NULL
            "#,
        )
        .bind(id)
        .bind(resolved_by)
        .bind(resolution_notes)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
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

    /// Bus identity for `INLINE_CTX` bookkeeping: the shared `inline_state`
    /// `Arc` is common to all clones of this bus and unique per logical bus.
    fn inline_bus_id(&self) -> usize {
        Arc::as_ptr(&self.inline_state) as usize
    }

    /// Synchronously dispatch `event` to every registered subscriber, in
    /// priority order. Used only by `DispatchMode::Inline`. See
    /// [`INLINE_CTX`] for why same-bus re-entrance queues in place while a
    /// cross-bus cascade is deferred rather than run immediately.
    pub(crate) async fn dispatch_inline(
        &self,
        event: Arc<Event<D>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let bus_id = self.inline_bus_id();
        let ctx = INLINE_CTX.try_with(Clone::clone).ok();

        match ctx {
            Some(ctx) if ctx.draining.contains(&bus_id) => {
                // Same-task, same-bus re-entrance: append and return. The
                // active drain of this bus (further up this task's stack)
                // will process it after the current handler returns.
                let mut state = self.inline_state.lock().await;
                state.queue.push_back(InlineQueueEntry {
                    event,
                    done: Arc::new(Notify::new()),
                });
                Ok(())
            }
            Some(ctx) => {
                // Cross-bus cascade: defer. The innermost currently-draining
                // bus's drain_owning loop picks this up once its own queue
                // empties (see INLINE_CTX for why this must not run
                // immediately).
                let bus = self.clone();
                ctx.pending_cross_bus
                    .lock()
                    .expect("pending_cross_bus mutex poisoned")
                    .push_back(Box::pin(async move { bus.drain_owning(event).await }));
                Ok(())
            }
            None => {
                // True top level for this task: become the drainer with a
                // fresh shared cross-bus queue.
                self.drain_owning(event).await
            }
        }
    }

    /// Becomes (or joins, via the existing `should_drive`/`Notify` wait) the
    /// drainer for this bus, pushes `event` onto its queue, then drains this
    /// bus's own queue and the shared cross-bus queue to quiescence —
    /// alternating between the two, since draining one can enqueue more work
    /// on the other, until a full pass finds both empty.
    ///
    /// Called both for the true top-level entry (fresh `pending_cross_bus`)
    /// and when a deferred cross-bus publish is popped and run (reusing the
    /// ambient `pending_cross_bus`, so the whole cascade tree rooted at one
    /// top-level dispatch shares a single queue).
    async fn drain_owning(
        &self,
        event: Arc<Event<D>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let bus_id = self.inline_bus_id();

        let done = Arc::new(Notify::new());
        let should_drive = {
            let mut state = self.inline_state.lock().await;
            state.queue.push_back(InlineQueueEntry {
                event,
                done: done.clone(),
            });
            if state.in_progress {
                false
            } else {
                state.in_progress = true;
                true
            }
        };

        if !should_drive {
            // Another task is already draining this bus. Wait for our entry
            // to be processed.
            done.notified().await;
            return Ok(());
        }

        // We own the drain. Extend (or start) the ambient INLINE_CTX with
        // this bus's identity, reusing the ambient pending_cross_bus if
        // we're nested under an outer drain so the whole cascade tree
        // shares one deferred-publish queue.
        let ambient = INLINE_CTX.try_with(Clone::clone).ok();
        let pending_cross_bus = ambient
            .as_ref()
            .map(|c| c.pending_cross_bus.clone())
            .unwrap_or_else(|| Arc::new(std::sync::Mutex::new(VecDeque::new())));
        let mut draining = ambient.map(|c| c.draining).unwrap_or_default();
        draining.push(bus_id);
        let ctx = InlineDispatchCtx {
            draining,
            pending_cross_bus: pending_cross_bus.clone(),
        };

        let projections = self.projections.clone();
        let inline_state = self.inline_state.clone();
        INLINE_CTX
            .scope(ctx, async move {
                loop {
                    // Drain this bus's own queue fully.
                    loop {
                        let entry = {
                            let mut state = inline_state.lock().await;
                            match state.queue.pop_front() {
                                Some(e) => e,
                                None => {
                                    state.in_progress = false;
                                    break;
                                }
                            }
                        };

                        // Snapshot the subscribers and sort by priority
                        // (projections before sagas). We can drop the `projections`
                        // lock before locking each observer to read its priority,
                        // and before invoking handlers, because re-entrant
                        // subscribes are not supported during dispatch (handlers
                        // may publish but not subscribe): a slow handler here would
                        // otherwise hold up every other caller of `subscribe()`/
                        // `start_listener()` for as long as `on_event` runs.
                        let sorted: Vec<Arc<Mutex<dyn EventObserver<D>>>> = {
                            let snapshot: Vec<_> =
                                { projections.lock().await.iter().cloned().collect() };
                            let mut tagged = Vec::with_capacity(snapshot.len());
                            for p in snapshot {
                                let priority = p.lock().await.priority();
                                tagged.push((priority, p));
                            }
                            tagged.sort_by_key(|(prio, _)| *prio);
                            tagged.into_iter().map(|(_, p)| p).collect()
                        };

                        for subscriber in sorted {
                            let guard = subscriber.lock().await;
                            // Contain a panicking observer (spec 0028 §3.6/R11):
                            // the inline path has no listener to survive, so a
                            // caught panic routes through this same `Err` branch,
                            // which runs the `inline_state` cleanup below
                            // (in_progress reset, queue cleared, waiters notified)
                            // — without it a swallowed panic would deadlock the bus.
                            let outcome =
                                match AssertUnwindSafe(guard.on_event(entry.event.clone()))
                                    .catch_unwind()
                                    .await
                                {
                                    Ok(result) => result,
                                    Err(payload) => Err(format!(
                                        "observer panicked: {}",
                                        panic_payload_message(payload)
                                    )
                                    .into()),
                                };
                            if let Err(e) = outcome {
                                // Notify waiters so they don't hang on error.
                                entry.done.notify_one();
                                let mut state = inline_state.lock().await;
                                state.in_progress = false;
                                state.queue.clear();
                                return Err(e);
                            }
                        }

                        entry.done.notify_one();
                    }

                    // Own queue is empty. Pop one deferred cross-bus publish
                    // (if any) and run it — it may itself enqueue more work
                    // on our own queue (same-bus reentrance from within its
                    // processing) or on pending_cross_bus again, so loop back
                    // around rather than assuming a single pass suffices.
                    let next = pending_cross_bus
                        .lock()
                        .expect("pending_cross_bus mutex poisoned")
                        .pop_front();
                    match next {
                        Some(fut) => fut.await?,
                        None => return Ok(()),
                    }
                }
            })
            .await
    }
}

/// Flushes `pending_checkpoint` if [`should_flush_checkpoint`] says the
/// configured threshold is met; a no-op otherwise. On flush failure the
/// pending checkpoint is put back so the next call retries, and the error is
/// logged with `context` (a short label identifying the caller for the log
/// line). Shared by the live-batch path (via `process_subscriber_for_batch`) so
/// the flush-and-retry-on-error dance can't drift between callers.
async fn try_flush_pending_checkpoint(
    pending_checkpoint: &mut Option<PendingCheckpoint>,
    context: &str,
    events_table: &str,
    subscriber_id: &str,
    mode: &CheckpointMode,
    pool: &PgPool,
    checkpoint_cache: &mut HashMap<String, u64>,
) {
    // Fold the publishable predicate INTO the `take_if` closure (spec 0027 §3.4):
    // applying it after the `take_if` would silently drop a pending already
    // moved out of the `Option`, losing its `first_event_time` / counter.
    let Some(pending) =
        pending_checkpoint.take_if(|p| p.is_publishable() && should_flush_checkpoint(p, mode))
    else {
        return;
    };
    if let Err(e) = flush_checkpoint(
        pool,
        events_table,
        subscriber_id,
        &pending,
        checkpoint_cache,
    )
    .await
    {
        error!(
            "{context}: failed to flush checkpoint for '{}': {}",
            subscriber_id, e
        );
        *pending_checkpoint = Some(pending);
    }
}

/// Records `subscriber_id -> mode` in the registry, warning if the id is
/// already present.
///
/// The listener seeds `subscriber_states` and the per-priority dispatch list
/// first-wins on a duplicate id (mod.rs `subscriber_states.contains_key`), so a
/// second `subscribe()` of an id already registered gets full catch-up history
/// and then never receives a live event: the first observer keeps driving the
/// subscriber silently. This is reachable through the documented `ReplayAlways`
/// re-subscribe path (spec 0024 §4.5 Correction 3), not just a copy-pasted id, so
/// it is a warning rather than an error — flagging it, not blocking it.
async fn warn_if_subscriber_id_reused(
    subscriber_modes: &Arc<Mutex<HashMap<String, SubscriptionMode>>>,
    subscriber_id: &str,
    mode: SubscriptionMode,
) {
    let previous = subscriber_modes
        .lock()
        .await
        .insert(subscriber_id.to_string(), mode);
    if previous.is_some() {
        warn!(
            "subscribe(): subscriber id '{}' is already registered on this bus. The \
             existing observer keeps receiving events; this new subscription will not \
             receive any live event (first-registration wins).",
            subscriber_id
        );
    }
}

/// Advances the contiguous-prefix checkpoint for one caught-up event (spec
/// 0026 R1/R3/R4), then flushes it according to the configured
/// `CheckpointMode` via [`try_flush_pending_checkpoint`] — a Synchronous pass
/// must durably persist after every advance, not just once at the end, or a
/// crash mid-pass redelivers the entire backlog instead of the "at most 1
/// event" the mode's rustdoc promises (spec 0026 §4.2).
///
/// Rows arrive in ascending `global_sequence` order, so the highest contiguous
/// prefix from where the pass started is a running counter, not a set. The
/// prefix advances only when the row exactly extends it (`seq == contiguous + 1`)
/// and freezes for the rest of the pass at the first hole: every later sequence
/// is `> contiguous + 1`, so the condition never fires again. This is why the
/// pass can never persist a checkpoint above a sequence that is still missing.
/// `pending_checkpoint` is only ever touched inside that same `seq ==
/// contiguous + 1` branch, so a flush is impossible unless a matching
/// sequence/event_id pair was set together (spec 0026 R4).
///
/// For a `ReplayAlways` subscriber it instead advances the in-memory high-water
/// mark and never touches the prefix, `pending_checkpoint`, or the checkpoints
/// table (spec 0026 §4.5, R6).
#[allow(clippy::too_many_arguments)]
async fn advance_catchup_prefix(
    replay_always: bool,
    hwm: &Arc<Mutex<HashMap<String, u64>>>,
    subscriber_id: &str,
    event_global_seq: u64,
    event_id: Uuid,
    contiguous: &mut u64,
    pending_checkpoint: &mut Option<PendingCheckpoint>,
    config: &ReliableDeliveryConfig,
    pool: &PgPool,
    checkpoint_cache: &mut HashMap<String, u64>,
) {
    if replay_always {
        hwm.lock()
            .await
            .insert(subscriber_id.to_string(), event_global_seq);
        return;
    }

    if event_global_seq != *contiguous + 1 {
        return;
    }

    *contiguous = event_global_seq;
    match pending_checkpoint {
        Some(p) => p.update(event_global_seq, event_id),
        None => *pending_checkpoint = Some(PendingCheckpoint::new(event_global_seq, event_id)),
    }

    try_flush_pending_checkpoint(
        pending_checkpoint,
        "Catch-up",
        &config.events_table,
        subscriber_id,
        &config.checkpoint_mode,
        pool,
        checkpoint_cache,
    )
    .await;
}

/// Runs one catch-up pass for a single subscriber.
///
/// For a `Checkpointed` subscriber this reads the persisted checkpoint, then
/// paginates `global_sequence > checkpoint` in `catch_up_batch_size` chunks,
/// dispatching each event through [`process_event_with_retry`].
///
/// The persisted checkpoint is the highest *contiguous* prefix from where the
/// pass started, tracked by [`advance_catchup_prefix`], not the maximum sequence
/// seen (spec 0026 R1). `global_sequence` is assigned by a non-transactional
/// `nextval()` (spec 0019), so a visible page can contain a hole a still-open
/// transaction fills in later; checkpointing the maximum would strand that
/// event below the seed of the live loop. The prefix freezes at the first hole
/// and is flushed according to the configured `CheckpointMode` as it advances
/// (an unconditional final flush covers a Batched pass that ends without
/// crossing its threshold) rather than once at the end regardless of mode,
/// which would let a Synchronous pass redeliver its entire backlog on a
/// mid-pass crash instead of honouring its "at most 1 event" durability
/// contract (spec 0026 §4.2). A hole that never fills leaves the checkpoint
/// below it, so the live loop re-reads from there and delivers it; the pass
/// still terminates because the pagination cursor keeps advancing by the
/// maximum sequence seen (§4.1, spec 0026 R3). Recovering a *permanent* hole
/// (one that will never fill) is the live listener's job, not this pass's:
/// catch-up has no gap fence, snapshot, or timeout backstop of its own (see
/// spec 0026 §3).
///
/// For a [`SubscriptionMode::ReplayAlways`] subscriber (spec 0024 R5) it
/// ignores the persisted checkpoint entirely, starting from the surviving
/// in-memory high-water mark (0 on a fresh `subscribe`, which resets it; the
/// retained value on a listener restart) and routing advancement to that HWM
/// instead of the checkpoints table, which is never written for such a
/// subscriber.
///
/// Returns `(pagination_cursor, contiguous_prefix)`. The cursor is the highest
/// `global_sequence` reached and is the correct `> cursor` lower bound for the
/// `subscribe()` buffer drain; the contiguous prefix is what was actually
/// persisted, so the drain can continue the *same* counter over the drained
/// range and never flush above the hole (§4.3, spec 0026 R2). Shared by
/// `subscribe` and the pre-loop pass in `start_listener` so the two catch-up
/// paths cannot drift apart.
///
/// `replay_always` is resolved once by the caller (it reads the observer's own
/// mutex) rather than re-derived here, so a single `subscribe()` call only
/// resolves the subscription mode once.
pub(crate) async fn catch_up_from_checkpoint<ED>(
    observer: &Arc<Mutex<dyn EventObserver<ED>>>,
    subscriber_id: &str,
    replay_always: bool,
    failure_mode: FailureMode,
    config: &ReliableDeliveryConfig,
    pool: &PgPool,
    hwm: &Arc<Mutex<HashMap<String, u64>>>,
) -> Result<(u64, u64), SqlxError>
where
    ED: EventData + Send + Sync + DeserializeOwned + 'static,
{
    // spec 0024 R5: a ReplayAlways subscriber ignores the persisted checkpoint
    // and starts from its surviving in-memory HWM, so a prior
    // fast_forward-to-head does not suppress replay.
    let last_sequence = if replay_always {
        hwm.lock().await.get(subscriber_id).copied().unwrap_or(0)
    } else {
        let result: Option<(i64,)> = sqlx::query_as(
            r#"
            SELECT last_global_sequence
            FROM epoch_event_bus_checkpoints
            WHERE bus_name = $1 AND subscriber_id = $2
            "#,
        )
        .bind(&config.events_table)
        .bind(subscriber_id)
        .fetch_optional(pool)
        .await?;

        result.map(|(seq,)| seq as u64).unwrap_or(0)
    };

    // Pagination cursor: advances by the maximum sequence seen so the pass
    // terminates at head even past an unfilled hole (§4.1).
    let mut current_sequence = last_sequence;
    let mut total_caught_up = 0u64;

    // Contiguous-prefix checkpoint (spec 0026 R1/R4): seeded from where the
    // pass started, advanced only across an unbroken run by
    // advance_catchup_prefix, which also flushes according to the configured
    // CheckpointMode after each advance (spec 0026 §4.2). The final flush
    // below is unconditional so a Batched pass never ends with an unflushed
    // advance.
    let mut contiguous = last_sequence;
    let mut pending_checkpoint: Option<PendingCheckpoint> = None;
    // Local checkpoint cache for flush_checkpoint
    let mut checkpoint_cache: HashMap<String, u64> = HashMap::new();

    // Process catch-up events from the database
    let subscriber_catchup_query = format!(
        "SELECT id, stream_id, stream_version, event_type, data, \
         created_at, actor_id, purger_id, purged_at, \
         global_sequence, causation_id, correlation_id, schema_version \
         FROM {} WHERE global_sequence > $1 \
         ORDER BY global_sequence ASC LIMIT $2",
        config.events_table,
    );
    // Fail-closed halt marker (spec 0028 R3). Set at the bad row, it breaks the
    // inner row loop AND the outer pagination loop: without the outer break a
    // full batch (batch_size == catch_up_batch_size) would re-fetch the same
    // window from the unchanged `current_sequence`, re-hit the same row, and
    // spin forever with no sleep, so `subscribe()` would never return.
    let mut halted = false;
    loop {
        let rows: Vec<PgDBEvent> = sqlx::query_as(&subscriber_catchup_query)
            .bind(current_sequence as i64)
            .bind(config.catch_up_batch_size as i64)
            .fetch_all(pool)
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

            let data: Option<ED> = match row.data.map(|d| serde_json::from_value(d)).transpose() {
                Ok(d) => d,
                Err(e) => match failure_mode {
                    // Fail-closed halt (spec 0028 R3, catch-up leg): hold the
                    // contiguous prefix below this sequence (skip the advance),
                    // write the deser DLQ row, fire on_halt, and break out of
                    // both loops via the halt marker. The final flush stays safe
                    // by construction — `pending_checkpoint` never advanced past
                    // the hole, so there is nothing above it to flush.
                    FailureMode::FailClosed => {
                        let error_message = format!("unrecoverable: deserialize: {}", e);
                        warn!(
                            "Catch-up: fail-closed halt for '{}': event {} (seq {}) is \
                             undeserializable: {}. Holding the checkpoint below this sequence \
                             until the payload is corrected or the subscriber is released.",
                            subscriber_id, event_id, event_global_seq, e
                        );
                        insert_deser_halt_dlq_row(
                            pool,
                            subscriber_id,
                            event_id,
                            event_global_seq,
                            &error_message,
                        )
                        .await;
                        fire_on_halt(
                            config,
                            subscriber_id,
                            event_global_seq,
                            HaltReason::DeserializeFailure,
                        )
                        .await;
                        halted = true;
                        break;
                    }
                    // Fail-open (unchanged): log, skip, advance past the
                    // undeserializable event to avoid an infinite retry loop; a
                    // skipped-past event counts as processed for the prefix.
                    _ => {
                        warn!(
                            "Catch-up: skipping event {} (type: '{}', global_seq: {}) for '{}': \
                                 failed to deserialize: {}. This is expected when event variants have \
                                 been removed from the application enum. Advancing checkpoint past this event.",
                            event_id, row.event_type, event_global_seq, subscriber_id, e
                        );
                        current_sequence = event_global_seq;
                        total_caught_up += 1;
                        advance_catchup_prefix(
                            replay_always,
                            hwm,
                            subscriber_id,
                            event_global_seq,
                            event_id,
                            &mut contiguous,
                            &mut pending_checkpoint,
                            config,
                            pool,
                            &mut checkpoint_cache,
                        )
                        .await;
                        continue;
                    }
                },
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
                // CLOUD-173: carry the stored schema version through the bus read path.
                // NULL (pre-migration rows) is interpreted as version 1.
                schema_version: row.schema_version.unwrap_or(1).max(0) as u32,
            });

            // Use the same retry/DLQ logic as real-time processing
            let result =
                process_event_with_retry(observer, &event, subscriber_id, config, pool).await;

            // Fail-closed observer-exhaustion halt (spec 0028 R3/R4, catch-up
            // leg): the DLQ row + on_dlq_insertion already fired inside the
            // retry ladder; add on_halt and hold below this sequence (skip the
            // advance, do not bump the cursor), breaking both loops via the
            // halt marker.
            if let ProcessResult::SentToDlq = result
                && failure_mode == FailureMode::FailClosed
            {
                warn!(
                    "Catch-up: fail-closed halt for '{}': observer exhausted retries on event \
                     {} (seq {}). Holding the checkpoint below this sequence until the observer \
                     recovers or the subscriber is released.",
                    subscriber_id, event_id, event_global_seq
                );
                fire_on_halt(
                    config,
                    subscriber_id,
                    event_global_seq,
                    HaltReason::ObserverFailure,
                )
                .await;
                halted = true;
                break;
            }

            advance_catchup_prefix(
                replay_always,
                hwm,
                subscriber_id,
                event_global_seq,
                event_id,
                &mut contiguous,
                &mut pending_checkpoint,
                config,
                pool,
                &mut checkpoint_cache,
            )
            .await;

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

        // A fail-closed halt exits the outer pagination loop too, or a full
        // batch would re-fetch the same window and spin forever (spec 0028 P3
        // P0 fix).
        if halted {
            break;
        }

        // If we got fewer events than the batch size, we've caught up
        if batch_size < config.catch_up_batch_size as usize {
            break;
        }
    }

    // Final unconditional flush (spec 0026 §4.2): per-advance flushing above
    // already durably persists a Synchronous pass and any Batched threshold
    // crossed mid-pass; this catches a Batched pass that ends without crossing
    // one, so it can never leave an advance unflushed. Nothing to do for a
    // ReplayAlways subscriber (routed to the HWM) or when the prefix never
    // advanced past the seed. `pending_checkpoint` is only ever set together
    // with its matching `event_id`, so a flushed `last_event_id` always
    // matches `last_global_sequence` (spec 0026 R4).
    if !replay_always
        && let Some(pending) = pending_checkpoint.take()
        && let Err(e) = flush_checkpoint(
            pool,
            &config.events_table,
            subscriber_id,
            &pending,
            &mut checkpoint_cache,
        )
        .await
    {
        error!(
            "Catch-up: failed to flush final checkpoint for '{}': {}",
            subscriber_id, e
        );
    }

    if total_caught_up > 0 {
        info!(
            "Catch-up complete for '{}': processed {} events, cursor at {}, checkpoint now at {}",
            subscriber_id, total_caught_up, current_sequence, contiguous
        );
    }

    Ok((current_sequence, contiguous))
}

impl<D> EventBus for PgEventBus<D>
where
    D: EventData + Send + Sync + DeserializeOwned + 'static,
{
    type EventType = D;
    type Error = PgEventBusError;

    /// In `DispatchMode::Async` this is a no-op: events flow through the
    /// event store -> trigger -> NOTIFY -> listener path. In
    /// `DispatchMode::Inline` it walks subscribers synchronously.
    fn publish<'a>(
        &'a self,
        event: Arc<Event<Self::EventType>>,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        match self.config.dispatch_mode {
            config::DispatchMode::Async => Box::pin(async { Ok(()) }),
            config::DispatchMode::Inline => Box::pin(async move {
                self.dispatch_inline(event)
                    .await
                    .map_err(PgEventBusError::InlineDispatchError)
            }),
        }
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
        let hwm = self.hwm.clone();
        let subscriber_modes = self.subscriber_modes.clone();

        let inline_state = self.inline_state.clone();
        Box::pin(async move {
            // Wrap the projector in Arc<Mutex<>> for sharing
            let observer: Arc<Mutex<dyn EventObserver<Self::EventType>>> =
                Arc::new(Mutex::new(projector));

            // Read id + mode once, while nothing else can be holding the
            // observer's lock. Recorded into `subscriber_modes` only on the
            // paths below that actually register the observer: a Coordinated-mode
            // subscribe that loses the advisory-lock race must NOT appear in the
            // registry, or `wait_until_all_caught_up` would poll a subscriber this
            // process never drives, burning its whole timeout every time (a
            // ReplayAlways position is a per-process HWM that would then never
            // advance here).
            let (subscriber_id, mode, failure_mode) = {
                let o = observer.lock().await;
                (
                    o.subscriber_id().to_string(),
                    o.subscription_mode(),
                    o.failure_mode(),
                )
            };
            let replay_always = mode == SubscriptionMode::ReplayAlways;

            // Inline dispatch: no LISTEN task, no NOTIFY channel, no catch-up.
            // Just register the subscriber and return. Any events published
            // before subscription are not replayed (intentional for tests).
            if config.dispatch_mode == config::DispatchMode::Inline {
                warn_if_subscriber_id_reused(&subscriber_modes, &subscriber_id, mode).await;
                // Touch inline_state so the field isn't considered unused on
                // the subscribe path; ensures the queue is initialized.
                let _ = inline_state.lock().await;
                let mut guard = projections.lock().await;
                guard.push(observer);
                return Ok(());
            }

            // R4 (defence-in-depth): in Async mode, delivery of newly committed
            // events depends on the AFTER INSERT NOTIFY trigger. If it is absent
            // (e.g. subscribe() called before start_listener/setup_trigger created
            // it), warn loudly so a misconfiguration is visible in logs rather than
            // silently relying on the 1s timer tick. We do not hard-error: the R2
            // catch-up pass and the timer fallback still deliver correctly.
            // The probe is for THIS bus's per-channel trigger: another bus's
            // trigger on the same table would not deliver to this channel.
            match notify_trigger_name(&pool, &channel_name).await {
                Ok(trigger_name) => {
                    match trigger_exists(&pool, &config.events_table, &trigger_name).await {
                        Ok(false) => warn!(
                            "Async subscribe for '{}' on bus '{}' (channel '{}'): no NOTIFY \
                             trigger '{}' on table '{}'. New events will be delivered only on \
                             the periodic timer tick until start_listener()/setup_trigger() \
                             creates it.",
                            subscriber_id,
                            config.events_table,
                            channel_name,
                            trigger_name,
                            config.events_table
                        ),
                        Ok(true) => {}
                        Err(e) => log::debug!(
                            "Could not probe for NOTIFY trigger on '{}': {}",
                            config.events_table,
                            e
                        ),
                    }
                }
                Err(e) => log::debug!("Could not derive NOTIFY trigger name: {}", e),
            }

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

            // Past the Coordinated-mode gate: this instance will actually drive
            // the subscriber, so it is safe to make it visible to readiness.
            warn_if_subscriber_id_reused(&subscriber_modes, &subscriber_id, mode).await;

            // A fresh subscribe of a ReplayAlways subscriber rebuilds its in-memory
            // model from empty, so reset the HWM before catch-up: readiness must not
            // report a stale high position from a previous subscription lifecycle
            // while the model is being rebuilt. Catch-up re-seeds it as it
            // progresses.
            //
            // Must sit after the Coordinated-mode gate, not before it. The HWM is
            // shared by every clone of this bus, so zeroing it on a subscribe that
            // then loses the advisory-lock race would knock the *winning*
            // subscription's readiness position back to 0 with nothing left to
            // advance it, reporting a healthy at-head subscriber as permanently
            // behind.
            if replay_always {
                hwm.lock().await.insert(subscriber_id.clone(), 0);
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
            let (buffer_tx, mut buffer_rx) = tokio::sync::mpsc::channel::<(Uuid, u64)>(buffer_size);

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
                            if let Ok(payload) =
                                serde_json::from_str::<NotifyPayload>(notification.payload())
                            {
                                let global_seq = payload.global_sequence.unwrap_or(0) as u64;
                                // Send to bounded channel - will apply backpressure if full
                                if buffer_tx.send((payload.id, global_seq)).await.is_err() {
                                    // Receiver dropped, catch-up is complete
                                    break;
                                }
                                log::debug!("Buffered event during catch-up: {:?}", payload.id);
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

            // Catch up from the persisted checkpoint. Reuses the same paginated,
            // retry/DLQ-backed pass that start_listener runs before entering its
            // loop (spec 0026 R2), so the two paths cannot drift.
            // `current_sequence` is the pagination cursor (max seen), the correct
            // `> current_sequence` lower bound for the drain below. `contiguous`
            // is what catch-up actually persisted; the drain continues this same
            // counter so it can never flush above a hole catch-up stopped at
            // (spec 0026 R2).
            let (mut current_sequence, mut contiguous) = catch_up_from_checkpoint(
                &observer,
                &subscriber_id,
                replay_always,
                failure_mode,
                &config,
                &pool,
                &hwm,
            )
            .await?;

            // Continues catch-up's contiguous-prefix counter over the drained
            // range (§4.3). advance_catchup_prefix flushes according to the
            // configured CheckpointMode as it advances (spec 0026 §4.2), same as
            // the catch-up pass above; `pending_checkpoint` is only ever set
            // together with its matching `event_id`, so a flushed `last_event_id`
            // always matches its sequence (spec 0026 R4).
            let mut pending_checkpoint: Option<PendingCheckpoint> = None;
            let mut checkpoint_cache: HashMap<String, u64> = HashMap::new();

            // Stop the buffer listener
            buffer_handle.abort();

            // Drain the buffer to find the highest sequence seen during catch-up.
            // Close the receiver to signal the sender to stop.
            buffer_rx.close();
            let mut max_buffered_seq = current_sequence;
            let mut buffered_count = 0usize;
            while let Some((_id, global_seq)) = buffer_rx.recv().await {
                buffered_count += 1;
                if global_seq > max_buffered_seq {
                    max_buffered_seq = global_seq;
                }
            }

            let mut processed_from_buffer = 0u64;
            // Fail-closed drain halt marker (spec 0028 R3, drain leg). Set at the
            // bad row, it breaks both the inner row loop and the outer
            // pagination loop (a full batch would otherwise re-fetch the same
            // window and spin forever). It must NOT early-return out of
            // subscribe(): registration (projections.push below) and the
            // post-drain flush still run, so the subscriber is driven by the
            // listener and can self-heal (R3, §3.3).
            let mut halted = false;

            // Only query if at least one buffered event is newer than our checkpoint.
            // The `global_sequence > current_sequence` predicate also performs the
            // dedup that the old in-memory loop did explicitly.
            //
            // Paginated like the catch-up loop above: fetch at most `catch_up_batch_size`
            // rows per query to bound per-iteration memory use even when many events
            // arrive during a long catch-up period.
            if max_buffered_seq > current_sequence {
                loop {
                    // A transient DB error here returns Err from subscribe(), which is
                    // safe under at-least-once semantics: checkpoints are durable and
                    // the caller can retry subscribe() to resume from where it left off.
                    let rows: Vec<PgDBEvent> = sqlx::query_as(&format!(
                        "SELECT id, stream_id, stream_version, event_type, data, \
                         created_at, actor_id, purger_id, purged_at, \
                         global_sequence, causation_id, correlation_id, schema_version \
                         FROM {} WHERE global_sequence > $1 AND global_sequence <= $2 \
                         ORDER BY global_sequence ASC LIMIT $3",
                        config.events_table,
                    ))
                    .bind(current_sequence as i64)
                    .bind(max_buffered_seq as i64)
                    .bind(config.catch_up_batch_size as i64)
                    .fetch_all(&pool)
                    .await?;

                    if rows.is_empty() {
                        break;
                    }

                    let batch_size = rows.len();

                    for row in rows {
                        let event_global_seq = row.global_sequence.unwrap_or(0) as u64;
                        let event_id = row.id;

                        // Deserialize, mirroring the catch-up loop's skip-on-error
                        // behaviour: undeserializable variants advance the checkpoint
                        // rather than looping forever.
                        //
                        // `None` means deserialization failed; the event is skipped but
                        // the checkpoint still advances (common path below).
                        let maybe_data: Option<Option<Self::EventType>> = match row
                            .data
                            .map(serde_json::from_value)
                            .transpose()
                        {
                            Ok(d) => Some(d),
                            Err(e) => match failure_mode {
                                // Fail-closed halt: hold below this sequence (skip
                                // the shared advance), write the deser DLQ row,
                                // fire on_halt, and break both loops.
                                FailureMode::FailClosed => {
                                    let error_message =
                                        format!("unrecoverable: deserialize: {}", e);
                                    warn!(
                                        "Buffer processing: fail-closed halt for '{}': event {} \
                                         (seq {}) is undeserializable: {}. Holding the checkpoint \
                                         below this sequence until the payload is corrected or the \
                                         subscriber is released.",
                                        subscriber_id, event_id, event_global_seq, e
                                    );
                                    insert_deser_halt_dlq_row(
                                        &pool,
                                        &subscriber_id,
                                        event_id,
                                        event_global_seq,
                                        &error_message,
                                    )
                                    .await;
                                    fire_on_halt(
                                        &config,
                                        &subscriber_id,
                                        event_global_seq,
                                        HaltReason::DeserializeFailure,
                                    )
                                    .await;
                                    halted = true;
                                    break;
                                }
                                // Fail-open (unchanged): log, skip, advance past.
                                _ => {
                                    warn!(
                                        "Buffer processing: skipping event {} (type: '{}', \
                                         global_seq: {}) for '{}': failed to deserialize: {}. \
                                         Advancing checkpoint past this event.",
                                        event_id,
                                        row.event_type,
                                        event_global_seq,
                                        subscriber_id,
                                        e
                                    );
                                    None
                                }
                            },
                        };

                        if let Some(data) = maybe_data {
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
                                // CLOUD-173: carry the stored schema version through the bus read path.
                                // NULL (pre-migration rows) is interpreted as version 1.
                                schema_version: row.schema_version.unwrap_or(1).max(0) as u32,
                            });

                            let result = process_event_with_retry(
                                &observer,
                                &event,
                                &subscriber_id,
                                &config,
                                &pool,
                            )
                            .await;

                            processed_from_buffer += 1;

                            // Fail-closed observer-exhaustion halt (drain leg):
                            // the DLQ row + on_dlq_insertion already fired inside
                            // the retry ladder; add on_halt and hold below this
                            // sequence by breaking before the shared advance.
                            if let ProcessResult::SentToDlq = result
                                && failure_mode == FailureMode::FailClosed
                            {
                                warn!(
                                    "Buffer processing: fail-closed halt for '{}': observer \
                                     exhausted retries on event {} (seq {}). Holding the \
                                     checkpoint below this sequence until the observer recovers \
                                     or the subscriber is released.",
                                    subscriber_id, event_id, event_global_seq
                                );
                                fire_on_halt(
                                    &config,
                                    &subscriber_id,
                                    event_global_seq,
                                    HaltReason::ObserverFailure,
                                )
                                .await;
                                halted = true;
                                break;
                            }

                            if let ProcessResult::Success = result {
                                log::debug!(
                                    "Processed buffered event {} for '{}'",
                                    event_id,
                                    subscriber_id
                                );
                            }
                        }

                        // Common checkpoint tracking — runs for both the deserialization-
                        // error path and the successful-processing path. Advances the
                        // contiguous prefix carried over from catch-up, or the in-memory
                        // HWM for a ReplayAlways subscriber (§4.3, §4.5).
                        advance_catchup_prefix(
                            replay_always,
                            &hwm,
                            &subscriber_id,
                            event_global_seq,
                            event_id,
                            &mut contiguous,
                            &mut pending_checkpoint,
                            &config,
                            &pool,
                            &mut checkpoint_cache,
                        )
                        .await;

                        current_sequence = event_global_seq;
                    }

                    // A fail-closed halt exits the outer pagination loop too
                    // (spec 0028 P3 P0 fix), through normal control flow so
                    // registration and the final flush below still run.
                    if halted {
                        break;
                    }

                    if batch_size < config.catch_up_batch_size as usize {
                        break;
                    }
                }
            }

            // Final unconditional flush after buffer processing (spec 0026
            // §4.2/§4.3, R2, R4). `flush_checkpoint` is a blind, non-monotonic
            // upsert, so flushing anything above `contiguous` would overwrite
            // catch-up's conservative checkpoint inside this same subscribe()
            // call — "flush only the contiguous value" is the requirement, not an
            // optimisation. Per-advance flushing above already durably persists a
            // Synchronous drain and any Batched threshold crossed mid-drain; this
            // catches a Batched drain that ends without crossing one.
            // `pending_checkpoint` is `Some` exactly when the drain advanced the
            // prefix since its last flush; when it never advanced (e.g. the
            // drained rows all sit above a hole catch-up stopped at) catch-up's
            // own flush already persisted `contiguous`.
            if !replay_always
                && let Some(pending) = pending_checkpoint.take()
                && let Err(e) = flush_checkpoint(
                    &pool,
                    &config.events_table,
                    &subscriber_id,
                    &pending,
                    &mut checkpoint_cache,
                )
                .await
            {
                error!(
                    "Buffer processing: failed to flush final checkpoint for '{}': {}",
                    subscriber_id, e
                );
            }

            if buffered_count > 0 {
                info!(
                    "Drained {} buffered notifications for '{}', processed {} events up to \
                     sequence {}, checkpoint now at {}",
                    buffered_count,
                    subscriber_id,
                    processed_from_buffer,
                    max_buffered_seq,
                    contiguous
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
    fn notify_payload_parses_slim_trigger_json() {
        // Exact shape emitted by `epoch_notify_event()` after migration m010.
        let id = Uuid::new_v4();
        let json = format!(
            r#"{{
                "id": "{id}",
                "stream_id": "stream-1",
                "stream_version": 7,
                "event_type": "OrderPlaced",
                "actor_id": "actor-1",
                "global_sequence": 42,
                "created_at": "2026-06-11T00:00:00Z"
            }}"#
        );

        let payload: NotifyPayload =
            serde_json::from_str(&json).expect("slim payload should deserialize");

        assert_eq!(payload.id, id);
        assert_eq!(payload.global_sequence, Some(42));
    }

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

    #[test]
    fn gap_timeout_entry_structure() {
        let entry = GapTimeoutEntry {
            id: Uuid::new_v4(),
            bus_name: "epoch_events".to_string(),
            subscriber_id: "projection:orders".to_string(),
            skipped_sequence: 99,
            gap_duration_ms: 6250,
            timed_out_at: chrono::Utc::now(),
            resolved_at: None,
            resolved_by: None,
            resolution_notes: None,
        };

        assert_eq!(entry.bus_name, "epoch_events");
        assert_eq!(entry.subscriber_id, "projection:orders");
        assert_eq!(entry.skipped_sequence, 99);
        assert_eq!(entry.gap_duration_ms, 6250);
        assert!(entry.resolved_at.is_none());
        assert!(entry.resolved_by.is_none());
        assert!(entry.resolution_notes.is_none());

        // Clone works
        let cloned = entry.clone();
        assert_eq!(entry.bus_name, cloned.bus_name);
        assert_eq!(entry.skipped_sequence, cloned.skipped_sequence);
    }

    // =========================================================================
    // Contiguous-prefix unit tests (Phase 3, spec 0026)
    // =========================================================================
    //
    // These tests exercise `catch_up_from_checkpoint` directly, so they need a
    // real database. Holes are created by consuming a sequence slot via
    // `SELECT nextval(...)` without inserting a row, so no transaction is held
    // open during the catch-up call and concurrent TRUNCATE operations in other
    // test binaries are never blocked. Integration tests truncate with
    // RESTART IDENTITY so the consumed slot is irrelevant to them; Phase 3
    // tests isolate themselves from the shared table via `cu_set_checkpoint`.
    // Each test gets a unique subscriber ID to stay independent.

    /// Minimal event type used only inside these unit tests.
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    struct CuTestEvent {
        v: u32,
    }
    impl epoch_core::event::EventData for CuTestEvent {
        fn event_type(&self) -> &'static str {
            "CuTestEvent"
        }
    }

    /// A noop observer that satisfies `EventObserver<CuTestEvent>` but does
    /// nothing with the events.
    struct CuNoop {
        id: String,
        mode: SubscriptionMode,
    }
    impl epoch_core::SubscriberId for CuNoop {
        fn subscriber_id(&self) -> &str {
            &self.id
        }
    }
    #[async_trait::async_trait]
    impl EventObserver<CuTestEvent> for CuNoop {
        async fn on_event(
            &self,
            _event: Arc<Event<CuTestEvent>>,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            Ok(())
        }
        fn subscription_mode(&self) -> SubscriptionMode {
            self.mode
        }
    }

    /// Returns a pool connected to the test database, with all migrations run.
    /// Returns `None` when the database is unreachable (CI without Postgres
    /// skips gracefully; set `EPOCH_REQUIRE_DB=1` to make unavailability fail).
    async fn cu_pool() -> Option<PgPool> {
        // Load epoch_pg/.env if present (mirrors common::try_get_pg_pool).
        let env_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".env");
        dotenvy::from_path(env_path).ok();
        let url = std::env::var("DATABASE_URL").unwrap_or_else(|_| {
            "postgres://postgres:postgres@localhost:5432/epoch_pg_test".to_string()
        });
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(std::time::Duration::from_secs(5))
            // See the same settings in tests/common: these bound a lock wait on
            // the shared epoch_events table so a sibling binary's held
            // transaction fails this test fast instead of hanging it forever.
            .after_connect(|conn, _meta| {
                Box::pin(async move {
                    // Separate statements: sqlx::query uses the extended
                    // protocol, which rejects multiple `;`-separated commands.
                    sqlx::query("SET lock_timeout = '30s'")
                        .execute(&mut *conn)
                        .await?;
                    sqlx::query("SET idle_in_transaction_session_timeout = '60s'")
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .ok();
        match pool {
            None => {
                if std::env::var("EPOCH_REQUIRE_DB").is_ok_and(|v| v == "1") {
                    panic!("EPOCH_REQUIRE_DB=1 but Postgres is unreachable");
                }
                None
            }
            Some(p) => {
                crate::Migrator::new(p.clone())
                    .run()
                    .await
                    .expect("migration failed");
                Some(p)
            }
        }
    }

    fn cu_observer(
        sub_id: String,
        mode: SubscriptionMode,
    ) -> Arc<Mutex<dyn EventObserver<CuTestEvent>>> {
        Arc::new(Mutex::new(CuNoop { id: sub_id, mode }))
    }

    fn cu_config(batch_size: u32) -> ReliableDeliveryConfig {
        ReliableDeliveryConfig {
            catch_up_batch_size: batch_size,
            ..Default::default()
        }
    }

    /// Creates a table shaped like `epoch_events` with its OWN sequence backing
    /// `global_sequence`, so a test can burn a `nextval()` to simulate a
    /// permanent hole without leaving one in the shared `epoch_events` table,
    /// which would stall every other test binary's catch-up and live loop
    /// (a brand-new subscriber with no planted checkpoint walks from sequence
    /// 0 and can hit it). `LIKE ... INCLUDING ALL` copies the DEFAULT
    /// expression byte-for-byte, so it still points at
    /// `epoch_events_global_sequence_seq` until repointed here; the new
    /// sequence is `OWNED BY` the column, so dropping the table drops it too.
    /// Caller is responsible for dropping the returned table at the end of the
    /// test.
    async fn cu_isolated_events_table(pool: &PgPool) -> String {
        let table = format!("cu_isolated_events_{}", Uuid::new_v4().simple());
        let seq = format!("{table}_seq");
        sqlx::query(&format!(
            "CREATE TABLE {table} (LIKE epoch_events INCLUDING ALL)"
        ))
        .execute(pool)
        .await
        .expect("create isolated events table");
        sqlx::query(&format!("CREATE SEQUENCE {seq}"))
            .execute(pool)
            .await
            .expect("create isolated sequence");
        sqlx::query(&format!(
            "ALTER TABLE {table} ALTER COLUMN global_sequence SET DEFAULT nextval('{seq}')"
        ))
        .execute(pool)
        .await
        .expect("repoint isolated table's global_sequence default");
        sqlx::query(&format!(
            "ALTER SEQUENCE {seq} OWNED BY {table}.global_sequence"
        ))
        .execute(pool)
        .await
        .expect("bind isolated sequence ownership");
        table
    }

    async fn cu_insert(pool: &PgPool, table: &str, stream_id: Uuid, version: i64) -> (Uuid, i64) {
        let id = Uuid::new_v4();
        let data = serde_json::to_value(CuTestEvent { v: version as u32 }).unwrap();
        let seq: i64 = sqlx::query_scalar(&format!(
            r#"INSERT INTO {table}
                   (id, stream_id, stream_version, event_type, data, created_at)
               VALUES ($1, $2, $3, 'CuTestEvent', $4, NOW())
               RETURNING global_sequence"#
        ))
        .bind(id)
        .bind(stream_id)
        .bind(version)
        .bind(&data)
        .fetch_one(pool)
        .await
        .expect("cu_insert");
        (id, seq)
    }

    async fn cu_read_checkpoint(pool: &PgPool, table: &str, sub_id: &str) -> Option<(u64, Uuid)> {
        sqlx::query_as::<_, (i64, Uuid)>(
            "SELECT last_global_sequence, last_event_id \
             FROM epoch_event_bus_checkpoints \
             WHERE bus_name = $1 AND subscriber_id = $2",
        )
        .bind(table)
        .bind(sub_id)
        .fetch_optional(pool)
        .await
        .expect("cu_read_checkpoint")
        .map(|(seq, id)| (seq as u64, id))
    }

    /// Pre-sets the persisted checkpoint for a subscriber so that
    /// `catch_up_from_checkpoint` starts from `seq` rather than 0.
    /// Using `s1 - 1` isolates a test from older events in the shared table.
    async fn cu_set_checkpoint(pool: &PgPool, table: &str, sub_id: &str, seq: u64) {
        sqlx::query(
            "INSERT INTO epoch_event_bus_checkpoints \
             (bus_name, subscriber_id, last_global_sequence, last_event_id, updated_at) \
             VALUES ($1, $2, $3, $4, NOW()) \
             ON CONFLICT (bus_name, subscriber_id) DO UPDATE SET \
                 last_global_sequence = EXCLUDED.last_global_sequence, \
                 last_event_id = EXCLUDED.last_event_id, \
                 updated_at = NOW()",
        )
        .bind(table)
        .bind(sub_id)
        .bind(seq as i64)
        .bind(Uuid::nil())
        .execute(pool)
        .await
        .expect("cu_set_checkpoint");
    }

    /// Verifies spec 0026 R4: `last_event_id` in the checkpoint row corresponds
    /// to the event that actually sits at `last_global_sequence` in the events
    /// table. Robust to interleaving: looks up the event by seq instead of
    /// comparing a hard-coded id captured before the pass.
    async fn cu_assert_r4(pool: &PgPool, table: &str, sub_id: &str) {
        let Some((cp_seq, cp_id)) = cu_read_checkpoint(pool, table, sub_id).await else {
            return;
        };
        // A concurrent TRUNCATE in another test binary can delete the event
        // row this checkpoint points at. Treat an absent row as inconclusive
        // (skip) rather than panicking on `fetch_one`.
        let Some(db_id) = sqlx::query_scalar::<_, Uuid>(&format!(
            "SELECT id FROM {table} WHERE global_sequence = $1"
        ))
        .bind(cp_seq as i64)
        .fetch_optional(pool)
        .await
        .expect("query event at checkpoint seq for R4 check") else {
            return;
        };
        assert_eq!(
            cp_id, db_id,
            "spec 0026 R4: last_event_id must correspond to last_global_sequence"
        );
    }

    // -------------------------------------------------------------------------
    // Test 1: hole stops the contiguous prefix, cursor still reaches head
    // (spec 0026 R1, R3)
    //
    // Runs on its own isolated events table (own sequence too): the
    // nextval() below burns a slot that is never filled, leaving a permanent
    // hole. On the shared epoch_events table that hole would stall any other
    // test binary's brand-new, checkpoint-less catch-up/live-loop pass that
    // happens to walk over it.
    // -------------------------------------------------------------------------
    #[tokio::test]
    async fn catchup_prefix_stops_at_hole() {
        let Some(pool) = cu_pool().await else {
            return;
        };
        let table = cu_isolated_events_table(&pool).await;
        let stream_id = Uuid::new_v4();
        let sub_id = format!("test:cu_hole:{}", Uuid::new_v4());
        let config = ReliableDeliveryConfig {
            events_table: table.clone(),
            ..cu_config(100)
        };
        let hwm = Arc::new(Mutex::new(HashMap::new()));
        let observer = cu_observer(sub_id.clone(), SubscriptionMode::Checkpointed);

        let (_, s1) = cu_insert(&pool, &table, stream_id, 1).await;
        // Anchor the catch-up pass to start just before our first event.
        cu_set_checkpoint(&pool, &table, &sub_id, s1 as u64 - 1).await;
        let (id2, s2) = cu_insert(&pool, &table, stream_id, 2).await;

        // Consume s3's sequence slot without inserting a row so it appears as
        // a gap to catch_up_from_checkpoint. Using nextval on this table's OWN
        // sequence rather than an open transaction avoids holding a table lock
        // that would block concurrent TRUNCATE calls in other test binaries,
        // and rather than the shared table's sequence avoids leaving a
        // permanent hole anyone else could stumble on.
        let s3: i64 = sqlx::query_scalar(&format!("SELECT nextval('{table}_seq')"))
            .fetch_one(&pool)
            .await
            .expect("consume hole sequence slot");

        let (_, s4) = cu_insert(&pool, &table, stream_id, 4).await;
        let (_, s5) = cu_insert(&pool, &table, stream_id, 5).await;

        let (cursor, contiguous) = catch_up_from_checkpoint(
            &observer,
            &sub_id,
            false,
            FailureMode::FailOpen,
            &config,
            &pool,
            &hwm,
        )
        .await
        .expect("catch_up_from_checkpoint");

        // Prefix must stop before the hole. This table is exclusive to this
        // test, so no concurrent process can extend the contiguous run; the
        // prefix must never cross s3 (spec 0026 R1).
        assert!(
            contiguous < s3 as u64,
            "prefix (={contiguous}) must stop below the hole (={s3})"
        );
        assert!(cursor >= s5 as u64, "cursor must reach at least s5");
        assert!(s3 > s2 && s3 < s4, "sequence ordering sanity check");

        let cp = cu_read_checkpoint(&pool, &table, &sub_id).await;
        assert!(
            cp.is_none_or(|(seq, _)| seq < s3 as u64),
            "checkpoint must not cross the hole"
        );
        cu_assert_r4(&pool, &table, &sub_id).await;
        let _ = (s1, id2);

        sqlx::query(&format!("DROP TABLE IF EXISTS {table}"))
            .execute(&pool)
            .await
            .expect("drop isolated events table");
    }

    // -------------------------------------------------------------------------
    // Test 2: hole on a non-final page — prefix does NOT resume after page turn
    // (spec 0026 R1)
    //
    // The hole is permanent (a nextval slot consumed, never inserted).
    // batch_size=2 means the hole falls on the boundary between pages, proving
    // the prefix counter survives a page transition without restarting. Runs
    // on its own isolated events table for the same reason as test 1.
    // -------------------------------------------------------------------------
    #[tokio::test]
    async fn catchup_multi_page_hole_prefix_does_not_resume() {
        let Some(pool) = cu_pool().await else {
            return;
        };
        let table = cu_isolated_events_table(&pool).await;
        let stream_id = Uuid::new_v4();
        let sub_id = format!("test:cu_multipage:{}", Uuid::new_v4());
        // batch_size=2: page 1 = [s1,s2], hole at s3, page 2 = [s4,s5]
        let config = ReliableDeliveryConfig {
            events_table: table.clone(),
            ..cu_config(2)
        };
        let hwm = Arc::new(Mutex::new(HashMap::new()));
        let observer = cu_observer(sub_id.clone(), SubscriptionMode::Checkpointed);

        let (_, s1) = cu_insert(&pool, &table, stream_id, 1).await;
        cu_set_checkpoint(&pool, &table, &sub_id, s1 as u64 - 1).await;
        let (id2, s2) = cu_insert(&pool, &table, stream_id, 2).await;

        // Consume s3's sequence slot without inserting a row (same technique
        // as test 1, on this test's own isolated sequence).
        let s3: i64 = sqlx::query_scalar(&format!("SELECT nextval('{table}_seq')"))
            .fetch_one(&pool)
            .await
            .expect("consume hole sequence slot");

        // Events on the second page (s3 is absent).
        let (_, s4) = cu_insert(&pool, &table, stream_id, 4).await;
        let (_, s5) = cu_insert(&pool, &table, stream_id, 5).await;

        let (cursor, contiguous) = catch_up_from_checkpoint(
            &observer,
            &sub_id,
            false,
            FailureMode::FailOpen,
            &config,
            &pool,
            &hwm,
        )
        .await
        .expect("catch_up_from_checkpoint");

        assert!(
            contiguous < s3 as u64,
            "prefix (={contiguous}) must not resume past the hole (={s3}) after a page boundary"
        );
        assert!(cursor >= s5 as u64, "cursor must reach at least s5");
        assert!(s3 > s2 && s3 < s4, "gap sanity check");

        let cp = cu_read_checkpoint(&pool, &table, &sub_id).await;
        assert!(
            cp.is_none_or(|(seq, _)| seq < s3 as u64),
            "checkpoint must not cross the hole"
        );
        cu_assert_r4(&pool, &table, &sub_id).await;
        let _ = (s1, id2);

        sqlx::query(&format!("DROP TABLE IF EXISTS {table}"))
            .execute(&pool)
            .await
            .expect("drop isolated events table");
    }

    // -------------------------------------------------------------------------
    // Test 3: positive control — no hole, checkpoint advances to head
    // (spec 0026 R1, R4)
    //
    // Without this test an implementation that never advances would pass the
    // two hole tests. The checkpoint must advance at least to s3, and
    // last_event_id must correspond to last_global_sequence (R4).
    // -------------------------------------------------------------------------
    #[tokio::test]
    async fn catchup_positive_control_no_hole() {
        let Some(pool) = cu_pool().await else {
            return;
        };
        let table = "epoch_events";
        let stream_id = Uuid::new_v4();
        let sub_id = format!("test:cu_positive:{}", Uuid::new_v4());
        let config = cu_config(100);
        let hwm = Arc::new(Mutex::new(HashMap::new()));
        let observer = cu_observer(sub_id.clone(), SubscriptionMode::Checkpointed);

        // Pre-set to s1-1 so catch-up only processes our three events, then
        // assert contiguous reached at least s3 (it would equal s3 in the
        // absence of concurrent inserts, or be higher if they are contiguous).
        let (_, s1) = cu_insert(&pool, table, stream_id, 1).await;
        cu_set_checkpoint(&pool, table, &sub_id, s1 as u64 - 1).await;
        let (_, _s2) = cu_insert(&pool, table, stream_id, 2).await;
        let (_, s3) = cu_insert(&pool, table, stream_id, 3).await;

        let (cursor, contiguous) = catch_up_from_checkpoint(
            &observer,
            &sub_id,
            false,
            FailureMode::FailOpen,
            &config,
            &pool,
            &hwm,
        )
        .await
        .expect("catch_up_from_checkpoint");

        // The cursor scans to the head regardless of gaps, so it must reach s3
        // whether or not the range is contiguous.
        assert!(
            cursor >= s3 as u64,
            "cursor (={cursor}) must reach at least s3 (={s3})"
        );

        // A concurrent binary can consume a sequence between our inserts (e.g.
        // an in-flight transaction), opening a gap that legitimately stops the
        // prefix early, or a TRUNCATE can delete our rows. Only assert the
        // prefix advanced when [s1, s3] is actually gap-free; otherwise the
        // positive control is inconclusive and we skip it.
        let present: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM epoch_events WHERE global_sequence BETWEEN $1 AND $2",
        )
        .bind(s1)
        .bind(s3)
        .fetch_one(&pool)
        .await
        .expect("count events in [s1, s3]");
        if present == s3 - s1 + 1 {
            assert!(
                contiguous >= s3 as u64,
                "prefix (={contiguous}) must advance at least to s3 (={s3})"
            );
            // spec 0026 R4: last_event_id must correspond to last_global_sequence.
            let cp = cu_read_checkpoint(&pool, table, &sub_id).await;
            assert!(cp.is_some(), "checkpoint must be written");
            cu_assert_r4(&pool, table, &sub_id).await;
        }
        let _ = s1;
    }

    // -------------------------------------------------------------------------
    // Test 4: ReplayAlways unchanged — HWM updated, checkpoints table untouched
    // (spec 0026 R6)
    //
    // For ReplayAlways, advance_catchup_prefix updates the in-memory HWM but
    // never touches `contiguous` or the checkpoints table. The returned
    // `contiguous` is the HWM seed (s1-1 pre-seeded here) not the max seq.
    // -------------------------------------------------------------------------
    #[tokio::test]
    async fn catchup_replay_always_unchanged() {
        let Some(pool) = cu_pool().await else {
            return;
        };
        let table = "epoch_events";
        let stream_id = Uuid::new_v4();
        let sub_id = format!("test:cu_replay:{}", Uuid::new_v4());
        let config = cu_config(100);
        let hwm: Arc<Mutex<HashMap<String, u64>>> = Arc::new(Mutex::new(HashMap::new()));
        let observer = cu_observer(sub_id.clone(), SubscriptionMode::ReplayAlways);

        let (_, _s1) = cu_insert(&pool, table, stream_id, 1).await;
        let (_, _s2) = cu_insert(&pool, table, stream_id, 2).await;
        let (_, s3) = cu_insert(&pool, table, stream_id, 3).await;

        // Pre-seed HWM so the pass starts just before our events (mirrors
        // cu_set_checkpoint for Checkpointed subscribers).
        hwm.lock().await.insert(sub_id.clone(), s3 as u64 - 3);

        let (cursor, _contiguous) = catch_up_from_checkpoint(
            &observer,
            &sub_id,
            true,
            FailureMode::FailOpen,
            &config,
            &pool,
            &hwm,
        )
        .await
        .expect("catch_up_from_checkpoint");

        // Cursor must reach at least s3.
        assert!(cursor >= s3 as u64, "cursor must reach at least s3");

        // The HWM must be updated to at least s3.
        assert!(
            hwm.lock().await.get(&sub_id).copied().unwrap_or(0) >= s3 as u64,
            "ReplayAlways must update the in-memory HWM to at least s3"
        );

        // The checkpoints table must NOT be written for a ReplayAlways subscriber.
        let cp = cu_read_checkpoint(&pool, table, &sub_id).await;
        assert!(
            cp.is_none(),
            "ReplayAlways must not write to the checkpoints table"
        );
    }
}
