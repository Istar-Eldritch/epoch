//! The P5 wedge-heal actor (spec 0031 Phase 4): the gated `HealRequest`
//! passed from the fire site, the per-family bookkeeping the actor owns, and
//! the actor task itself.

use super::config::{
    ReliableDeliveryConfig, WedgeHealPolicy, WedgeRetiredCallback, WedgeRetiredInfo,
};
use super::unsubscribe::unsubscribe_core;
use super::{Projections, SubscriberRegistry, panic_payload_message};
use epoch_core::event::EventData;
use epoch_core::prelude::EventObserver;
use log::{error, warn};

use futures::FutureExt;
use sqlx::postgres::PgPool;
use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tokio::time::Duration;

/// A gated `GapUnproven` wedge, sent from the fire site
/// (`process_subscriber_for_batch`) to the bus's single heal-actor task (spec
/// 0031 Phase 4). The fire site only gate-checks and emits: it does no
/// unsubscribe/mint/callback work itself.
#[derive(Clone)]
pub(crate) struct HealRequest {
    /// The wedged subscriber id (either the unsuffixed boot id or a previous
    /// `{base}#gen{N}` heal-generation id).
    pub(crate) subscriber_id: String,
    /// The sequence `subscriber_id` is held below.
    pub(crate) held_below_sequence: u64,
}

/// Per-family in-memory bookkeeping for the P5 wedge-heal actor (spec 0031
/// Phase 4): the next generation number to mint, how many heal-generation
/// re-halts this family has already had (drives the backoff index), and
/// whether the family has hit its `max_generations` cap and stopped healing.
struct HealFamilyState {
    next_generation: u32,
    rehalt_count: u32,
    retired: bool,
}

/// One id's registry handles, snapshotted at heal-defer time and re-checked
/// by `Arc::ptr_eq` against the live registry when the backoff elapses (see
/// [`run_heal_actor`]).
type RegistrySnapshot<D> = Vec<Arc<Mutex<dyn EventObserver<D>>>>;

/// A `HealRequest` deferred behind its family's re-halt backoff: the request
/// itself, the instant its backoff elapses, and the registry snapshot taken
/// when it was deferred.
type DeferredHeal<D> = (HealRequest, tokio::time::Instant, RegistrySnapshot<D>);

/// Splits a wedged subscriber id into its wedge-heal family base and the
/// generation the id itself represents (spec 0031 R13).
///
/// Family match is exact: either `subscriber_id` IS the base (an unsuffixed
/// boot id, generation 1), or it is exactly `{base}#gen{digits}` (a
/// heal-generation id). Near-miss rejection falls out of requiring the ENTIRE
/// suffix after the last `#gen` to be non-empty ASCII digits: `base-suffix`
/// has no `#gen` substring at all, and `base#gen2extra`'s suffix `2extra` is
/// not all-digit, so both fall through to being treated as their OWN,
/// independent base at generation 1 rather than being folded into `base`'s
/// family.
pub(crate) fn wedge_family(subscriber_id: &str) -> (String, u32) {
    if let Some(idx) = subscriber_id.rfind("#gen") {
        let (base, marker) = subscriber_id.split_at(idx);
        let digits = &marker["#gen".len()..];
        if !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
            && let Ok(generation) = digits.parse::<u32>()
        {
            return (base.to_string(), generation);
        }
    }
    (subscriber_id.to_string(), 1)
}

/// The bus's single heal-actor task (spec 0031 Phase 4, R14): owns every
/// piece of state the fire site cannot reach — the per-family generation
/// counters and the backoff schedule — and performs the actual
/// unsubscribe/mint/callback work the fire site's gate only requests over the
/// channel. Receives `HealRequest`s from the fire-site gate and, per request:
/// retires the wedged id (via the narrowed-handle [`unsubscribe_core`], never
/// a bus clone), mints the fresh `{base}#gen{N}` id, invokes the application
/// callback, and logs. Spawned by `start_listener` only when
/// `on_wedge_retired` is configured; ends when its request-channel `rx`
/// closes, which happens once every clone of the paired sender has dropped —
/// the sender lives inside the listener task's own `async move` block, so it
/// drops when that task itself returns on `shutdown()` (see
/// [`PgEventBus::shutdown`](super::PgEventBus::shutdown)).
///
/// Termination: the `shutdown_rx` watch channel (shared with the listener
/// task) interrupts the loop immediately — including mid-backoff — and
/// `shutdown()` joins this task's handle alongside the listener's. A request
/// for a family whose re-halt backoff has not yet elapsed is DEFERRED (never
/// blocks the loop): other families' immediate boot-generation heals proceed
/// while the backing-off family waits, and the deferred request is processed
/// when its backoff elapses (or dropped at shutdown — the bus is going away).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_heal_actor<D>(
    mut rx: mpsc::UnboundedReceiver<HealRequest>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    callback: Arc<dyn WedgeRetiredCallback>,
    policy: WedgeHealPolicy,
    projections: Projections<D>,
    subscriber_modes: Arc<Mutex<SubscriberRegistry<D>>>,
    hwm: Arc<Mutex<HashMap<String, u64>>>,
    pending_delivered_sets: Arc<Mutex<HashMap<String, HashSet<u64>>>>,
    retired_ids: Arc<Mutex<HashSet<String>>>,
    pool: PgPool,
    config: ReliableDeliveryConfig,
) where
    D: EventData + Send + Sync + 'static,
{
    let mut families: HashMap<String, HealFamilyState> = HashMap::new();
    // Requests deferred behind a family's re-halt backoff: (request, due-at,
    // registry snapshot at defer-time). Deferral keeps one family's backoff
    // from serializing every other family's immediate boot-generation heal
    // behind it (per-family policy, single actor). Deduped by `subscriber_id`:
    // a second halt on an already-deferred id is ignored rather than pushing
    // a second entry — two halts on one wedged id before its backoff elapses
    // must mint at most one fresh generation and invoke the callback at most
    // once. The snapshot is the id's registry handles at the moment it was deferred;
    // it is re-checked by `Arc::ptr_eq` against the registry when the
    // backoff fires, so a request whose id was retired or re-subscribed
    // during the backoff window is dropped as stale rather than acted on.
    let mut deferred: Vec<DeferredHeal<D>> = Vec::new();

    loop {
        let next_due = deferred.iter().map(|(_, due, _)| *due).min();
        tokio::select! {
            changed = shutdown_rx.changed() => {
                // Shutdown signalled (or the channel closed): stop now. Any
                // deferred requests are dropped — the bus is going away.
                let _ = changed;
                break;
            }
            maybe_request = rx.recv() => {
                match maybe_request {
                    None => break,
                    Some(request) => {
                        if deferred
                            .iter()
                            .any(|(deferred_request, _, _)| {
                                deferred_request.subscriber_id == request.subscriber_id
                            })
                        {
                            log::debug!(
                                "wedge heal: '{}' already deferred pending its re-halt \
                                 backoff; ignoring this duplicate halt",
                                request.subscriber_id
                            );
                        } else if let Some((delay, snapshot)) = process_heal_request(
                            request.clone(),
                            &mut families,
                            &policy,
                            &callback,
                            &projections,
                            &subscriber_modes,
                            &hwm,
                            &pending_delivered_sets,
                            &retired_ids,
                            &pool,
                            &config,
                            false,
                        )
                        .await
                        {
                            deferred.push((request, tokio::time::Instant::now() + delay, snapshot));
                        }
                    }
                }
            }
            _ = async {
                    match next_due {
                        Some(due) => tokio::time::sleep_until(due).await,
                        None => std::future::pending::<()>().await,
                    }
                }, if next_due.is_some() => {
                let now = tokio::time::Instant::now();
                let mut due_requests = Vec::new();
                deferred.retain(|(req, due, snapshot)| {
                    if *due <= now {
                        due_requests.push((req.clone(), snapshot.clone()));
                        false
                    } else {
                        true
                    }
                });
                for (request, snapshot) in due_requests {
                    // Re-validate against the registry before acting: if the
                    // id was retired (no entry) or re-subscribed (a fresh set
                    // of handles under the same id) during the backoff
                    // window, this deferred request is stale — drop it
                    // without acting rather than retiring/minting for a
                    // subscription the fire site never observed wedged.
                    let still_valid = {
                        let registry = subscriber_modes.lock().await;
                        match registry.get(&request.subscriber_id) {
                            Some((_, handles)) => {
                                handles.len() == snapshot.len()
                                    && handles
                                        .iter()
                                        .zip(snapshot.iter())
                                        .all(|(a, b)| Arc::ptr_eq(a, b))
                            }
                            None => false,
                        }
                    };
                    if !still_valid {
                        log::debug!(
                            "wedge heal: deferred request for '{}' is stale (retired or \
                             re-subscribed during the backoff window); dropping without \
                             acting",
                            request.subscriber_id
                        );
                        continue;
                    }
                    // Re-processed with `from_deferred = true`: the backoff has
                    // elapsed, so the heal proceeds immediately (never
                    // re-defers — the backoff decision is skipped on this
                    // path).
                    process_heal_request(
                        request,
                        &mut families,
                        &policy,
                        &callback,
                        &projections,
                        &subscriber_modes,
                        &hwm,
                        &pending_delivered_sets,
                        &retired_ids,
                        &pool,
                        &config,
                        true,
                    )
                    .await;
                }
            }
        }
    }
}

/// Handles one heal request: family bookkeeping (generation floor from the
/// wedged id's own generation, cap, retirement), the mint, the retirement,
/// the callback, and the WARN. Returns `Some((backoff, registry_snapshot))`
/// when the family must back off before this request can proceed (the caller
/// defers it; the snapshot is the id's current registry handles, captured
/// here so the caller can re-validate against them when the backoff
/// elapses), `None` when the request was fully handled.
#[allow(clippy::too_many_arguments)]
async fn process_heal_request<D>(
    request: HealRequest,
    families: &mut HashMap<String, HealFamilyState>,
    policy: &WedgeHealPolicy,
    callback: &Arc<dyn WedgeRetiredCallback>,
    projections: &Projections<D>,
    subscriber_modes: &Arc<Mutex<SubscriberRegistry<D>>>,
    hwm: &Arc<Mutex<HashMap<String, u64>>>,
    pending_delivered_sets: &Arc<Mutex<HashMap<String, HashSet<u64>>>>,
    retired_ids: &Arc<Mutex<HashSet<String>>>,
    pool: &PgPool,
    config: &ReliableDeliveryConfig,
    from_deferred: bool,
) -> Option<(Duration, RegistrySnapshot<D>)>
where
    D: EventData + Send + Sync + 'static,
{
    let (base, wedged_generation) = wedge_family(&request.subscriber_id);
    let family = families
        .entry(base.clone())
        .or_insert_with(|| HealFamilyState {
            // Floor the counter on the wedged id's own generation: a request
            // arriving from an already-healed id (e.g. after a listener
            // restart re-spawned this actor with empty counters, or any
            // out-of-order delivery) must never mint a generation that
            // already existed — its checkpoint/DLQ rows are retained (step
            // (g)) and a colliding fresh subscribe onto a stale name would
            // inherit them. The floor also keeps the cap honest across
            // restarts.
            next_generation: 2.max(wedged_generation.saturating_add(1)),
            rehalt_count: 0,
            retired: false,
        });
    family.next_generation = family
        .next_generation
        .max(wedged_generation.saturating_add(1));

    // Cap already reached for this family in an earlier request: no
    // further heals, and no repeat ERROR spam (R14).
    if family.retired {
        return None;
    }

    // Generation cap (R14): retire the chain instead of minting beyond
    // `max_generations`. No callback — there is no fresh generation to
    // hand the caller.
    if family.next_generation > policy.max_generations {
        family.retired = true;
        if let Err(e) = unsubscribe_core(
            &request.subscriber_id,
            projections,
            subscriber_modes,
            hwm,
            pending_delivered_sets,
            retired_ids,
            pool,
            config,
            None,
        )
        .await
        {
            warn!(
                "wedge heal: cap-retirement unsubscribe of '{}' (family '{}') failed: {}; \
                 the family still stops healing",
                request.subscriber_id, base, e
            );
        }
        error!(
            "Wedge heal for family '{}' reached max_generations ({}): retired '{}' \
             permanently — no further heals for this family",
            base, policy.max_generations, request.subscriber_id
        );
        return None;
    }

    // Backoff (R14): a boot-generation wedge (the unsuffixed base) heals
    // immediately; a heal-generation re-halt waits the configured
    // schedule, capped at its last entry. A deferred request re-enters with
    // `from_deferred = true`: its backoff has already elapsed.
    let delay = if from_deferred || wedged_generation <= 1 {
        if wedged_generation <= 1 {
            family.rehalt_count = 0;
        }
        Duration::ZERO
    } else {
        let idx = (family.rehalt_count as usize).min(policy.rehalt_backoff.len().saturating_sub(1));
        let d = policy
            .rehalt_backoff
            .get(idx)
            .copied()
            .unwrap_or(Duration::ZERO);
        family.rehalt_count += 1;
        d
    };
    if !delay.is_zero() {
        let snapshot = subscriber_modes
            .lock()
            .await
            .get(&request.subscriber_id)
            .map(|(_, handles)| handles.clone())
            .unwrap_or_default();
        return Some((delay, snapshot));
    }

    let generation = family.next_generation;
    family.next_generation += 1;

    // (ii) Retire the wedged observer via the same removal `unsubscribe`
    // performs.
    if let Err(e) = unsubscribe_core(
        &request.subscriber_id,
        projections,
        subscriber_modes,
        hwm,
        pending_delivered_sets,
        retired_ids,
        pool,
        config,
        None,
    )
    .await
    {
        warn!(
            "wedge heal: unsubscribe of '{}' failed: {}; invoking the callback for \
             generation {} anyway (the wedged id may still linger until a retry)",
            request.subscriber_id, e, generation
        );
    }

    // (iv) WARN once per heal (old id, new id, generation, held-below) —
    // logged BEFORE the callback so a slow or hanging consumer cannot
    // suppress the operator's record of a heal that already happened (the
    // retirement above is done regardless of callback outcome).
    warn!(
        "Wedge heal: retired '{}' (family '{}'), generation {} minted as '{}#gen{}', \
         held below seq {}",
        request.subscriber_id, base, generation, base, generation, request.held_below_sequence
    );

    // (iii) Invoke the application callback, panic-contained (mirroring
    // `fire_on_halt`): a panicking callback never unwinds the actor task,
    // and the retirement above already happened regardless of outcome.
    let info = WedgeRetiredInfo {
        base_subscriber_id: base.clone(),
        retired_subscriber_id: request.subscriber_id.clone(),
        generation,
        held_below_sequence: request.held_below_sequence,
    };
    if let Err(payload) = AssertUnwindSafe(callback.on_wedge_retired(info))
        .catch_unwind()
        .await
    {
        warn!(
            "on_wedge_retired callback panicked for '{}' -> generation {}: {}. The panic \
             is contained; the wedged id remains retired.",
            request.subscriber_id,
            generation,
            panic_payload_message(payload)
        );
    }
    None
}
