//! Retire-and-removal-inventory mechanism backing
//! [`PgEventBus::unsubscribe`](super::PgEventBus::unsubscribe) (spec 0031
//! Phases 2-4): the narrowed-handle core, its projections-removal helper, and
//! the pool-only advisory-lock release it composes into.

use super::{ListenerState, PgEventBusError, Projections, SubscriberRegistry, config};
use epoch_core::event::EventData;
use epoch_core::prelude::EventObserver;
use log::warn;
use sqlx::Error as SqlxError;
use sqlx::postgres::PgPool;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Removes every Arc in `captured` from the projections Vec, matching by
/// [`Arc::ptr_eq`] under the outer `projections` mutex, preserving the order of
/// the remaining observers. Returns the number of Arcs removed.
///
/// # Capture-based contract (spec 0031 Phase 2)
///
/// `captured` must already hold the caller's own clones of the target id's Arcs,
/// taken out of the `subscriber_modes` registry entry while holding the
/// registry's mutex. This helper deliberately does **not** look the id up in the
/// registry and does **not** drop the registry entry — both are the caller's
/// responsibility, in that order (capture the Arcs first, then drop the entry),
/// because a caller that drops the entry first leaves nothing for a lookup to
/// find. Callers therefore cannot be broken by a future re-shape of the
/// registry's value type; only this helper and the registry's writers ever
/// touch that shape.
///
/// Removal is safe under the outer mutex: wake, catch-up, inline and
/// fast-forward paths clone the `Arc`s out of the Vec under the lock and drop
/// the guard before dispatching, so nothing holds the outer mutex across an
/// `on_event` await.
///
/// Written in Phase 2 per spec 0031; first production caller is Phase 3's
/// `unsubscribe` (which captures the id's Arcs before dropping the registry
/// entry). The unit test below additionally pins the pointer-identity
/// contract (a fresh same-id Arc must NOT match).
///
/// Lock ordering: this helper takes the projections mutex internally. Callers
/// that also touch the registry must DROP the registry guard before calling —
/// Phase 3's `unsubscribe` captures the `Vec` under the registry guard, drops
/// that guard, then calls this helper — so no path ever holds both mutexes
/// (spec 0031 Phase 2 review, cycle 1).
pub(crate) async fn remove_captured_observers<D>(
    projections: &Projections<D>,
    captured: &[Arc<Mutex<dyn EventObserver<D>>>],
) -> usize
where
    D: EventData + Send + Sync,
{
    let mut removed = 0;
    projections.lock().await.retain(|live| {
        let drop_it = captured.iter().any(|gone| Arc::ptr_eq(gone, live));
        if drop_it {
            removed += 1;
        }
        !drop_it
    });
    removed
}

/// Narrowed-handle core of [`PgEventBus::unsubscribe`](super::PgEventBus::unsubscribe) (spec 0031 Phase 4):
/// takes only the sub-handles the removal touches — never a full bus clone —
/// so the P5 heal actor can retire a wedged id without holding a `PgEventBus`
/// clone (which would keep the pool/registry/hwm alive independently of the
/// caller's own handle, exactly the leak the heal actor's lifecycle exists to
/// prevent). The inherent [`PgEventBus::unsubscribe`](super::PgEventBus::unsubscribe) delegates to this
/// function with its own fields; see that method's rustdoc for the full
/// removal-inventory and timing contract this implements.
///
/// `listener_state` preserves step (h)'s original placement: the
/// `listener_running` sample is taken at the marker-insert step, AFTER the DB
/// work, not up front — an `unsubscribe` racing a concurrent
/// `start_listener` must not miss its tombstone marker. The inherent method
/// passes `Some(&self.listener_state)`; the heal actor passes `None` (it only
/// exists between `start_listener` spawning it and `shutdown()` joining it —
/// a window in which a listener is, by construction, always running).
///
/// Narrowing note: the handles here are STRUCTURAL — they are the only
/// state this function touches — but the carried `ReliableDeliveryConfig`
/// transitively holds the application's callback Arc, which in a realistic
/// consumer may itself hold a `PgEventBus` clone. The narrowing guarantees
/// the *removal logic* never needs a full bus handle; it cannot guarantee
/// that the actor's memory footprint is bus-free when a consumer's callback
/// closes over one.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn unsubscribe_core<D>(
    subscriber_id: &str,
    projections: &Projections<D>,
    subscriber_modes: &Arc<Mutex<SubscriberRegistry<D>>>,
    hwm: &Arc<Mutex<HashMap<String, u64>>>,
    pending_delivered_sets: &Arc<Mutex<HashMap<String, HashSet<u64>>>>,
    retired_ids: &Arc<Mutex<HashSet<String>>>,
    pool: &PgPool,
    config: &config::ReliableDeliveryConfig,
    listener_state: Option<&Arc<Mutex<Option<ListenerState>>>>,
) -> Result<bool, PgEventBusError>
where
    D: EventData + Send + Sync + 'static,
{
    // (a) Capture the id's handles under the registry guard, then drop the
    // guard BEFORE touching the projections Vec — lock-ordering contract on
    // `remove_captured_observers`: no path holds both mutexes.
    let captured: Option<Vec<Arc<Mutex<dyn EventObserver<D>>>>> = {
        let mut registry = subscriber_modes.lock().await;
        registry.remove(subscriber_id).map(|(_, handles)| handles)
    };

    // Idempotency (R4, OQ-2): an unknown id is `Ok(false)`.
    let Some(captured) = captured else {
        return Ok(false);
    };

    // (b) Remove every captured Arc from the projections Vec by `Arc::ptr_eq`.
    let removed_handles = remove_captured_observers(projections, &captured).await;
    log::debug!(
        "unsubscribe: removed {removed_handles} of {} captured handle(s) for \
         '{subscriber_id}' from the projections Vec",
        captured.len()
    );

    // (d) Remove listener-visible per-id state: the ReplayAlways HWM and the
    // delivered-set handoff carrier.
    hwm.lock().await.remove(subscriber_id);
    pending_delivered_sets.lock().await.remove(subscriber_id);

    // (e) Resolve unresolved gap-timeout rows for this subscriber (R8).
    let resolved = sqlx::query(
        r#"
        UPDATE epoch_event_bus_gap_timeouts
        SET resolved_at = NOW(),
            resolved_by = 'unsubscribe'
        WHERE bus_name = $1
          AND subscriber_id = $2
          AND resolved_at IS NULL
        "#,
    )
    .bind(&config.events_table)
    .bind(subscriber_id)
    .execute(pool)
    .await?;
    if resolved.rows_affected() > 0 {
        log::debug!(
            "unsubscribe: resolved {} gap-timeout row(s) for '{subscriber_id}'",
            resolved.rows_affected()
        );
    }

    // (f) Coordinated mode: best-effort advisory-lock release (R10).
    if config.instance_mode == config::InstanceMode::Coordinated {
        match release_subscriber_lock_core(pool, subscriber_id).await {
            Ok(true) => {}
            Ok(false) => log::debug!(
                "unsubscribe: advisory lock for '{subscriber_id}' was not held by this \
                 session (already released or held elsewhere); continuing",
            ),
            Err(e) => warn!(
                "unsubscribe: best-effort advisory-lock release for '{subscriber_id}' \
                 failed (continuing; the session-scoped lock is released when its \
                 holding session ends): {e}",
            ),
        }
    }

    // (g) Checkpoint and DLQ rows are deliberately RETAINED (R8). No DELETE here.

    // (h) Retired-id tombstone marker, LAST — only when a listener is
    // running. The sample is taken HERE (after the DB work), preserving the
    // committed placement: a retire racing a concurrent start_listener must
    // not miss its marker.
    let listener_running = match listener_state {
        Some(state) => state.lock().await.is_some(),
        // The heal actor's window always has a running listener.
        None => true,
    };
    if listener_running {
        retired_ids.lock().await.insert(subscriber_id.to_string());
    }

    Ok(true)
}

/// Pool-only core of [`PgEventBus::release_subscriber_lock`](super::PgEventBus::release_subscriber_lock),
/// also used by [`unsubscribe_core`]'s best-effort Coordinated-mode release:
/// needs nothing but the pool, so it composes into `unsubscribe_core`'s
/// narrowed handles without requiring a bus clone.
pub(crate) async fn release_subscriber_lock_core(
    pool: &PgPool,
    subscriber_id: &str,
) -> Result<bool, SqlxError> {
    let result: (bool,) = sqlx::query_as(
        r#"
        SELECT pg_advisory_unlock(
            ('x' || substr(md5($1), 1, 8))::bit(32)::int,
            ('x' || substr(md5($1), 9, 8))::bit(32)::int
        )
        "#,
    )
    .bind(subscriber_id)
    .fetch_one(pool)
    .await?;

    Ok(result.0)
}
