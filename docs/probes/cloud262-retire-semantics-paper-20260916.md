# CLOUD-262 probe: unsubscribe/retire semantics paper (2026-09-16)

Repo: `epoch` worktree `cloud-262`, rev `5c27432` (CLOUD-261 merged to main).
Deliverable class: **semantics paper — pure analysis**. No code changed, no Postgres
probes run. Every anchor below was verified by reading the cited lines at this rev.

---

## 1. Summary (verdicts up front)

| # | Verdict |
|---|---------|
| V1 | An unsubscribe that is "safe on wedged subscribers" **must not scan observers by locking them**. `invoke_observer_once` holds the observer mutex across the whole `on_event` await (`retry.rs:43-60`), so today's only identification path (lock each observer to read `subscriber_id`, as `fast_forward_all_subscribers` does at `mod.rs:3026-3035`) blocks indefinitely behind a wedged handler. A retire API needs an id→observer registry (the same reason `subscriber_modes` exists, `mod.rs:1424-1437`). |
| V2 | Effect timing is **"from the next wake"**, not immediate. Since 2bf9512 the select loop snapshots `projections` then releases the lock (`mod.rs:2036-2041`); the current wake keeps driving a retired observer through `Arc` clones for the rest of the wake — under a long drain that is the entire remaining backlog, and it keeps flushing the retired id's checkpoints the whole time. No fence exists against this. |
| V3 | **The per-wake state maps are not per-wake.** `checkpoint_cache`, `pending_checkpoints`, `subscriber_states`, `last_event_ids` are declared once per listener task (`mod.rs:1872-1885`), outside both the reconnect loop (1890) and the wake loop; they live until the listener task dies. Unsubscribe cannot clean them from the bus struct. Consequences: one wasted fetch per wake (floor pinning), a benign no-op in the private wedge fetch, and — load-bearing for design — **shutdown/reconnect `flush_all_pending_checkpoints` can resurrect a deleted checkpoint row** (`mod.rs:1892-1902`, 1965-1975). |
| V4 | **Retain checkpoint rows; do not delete at retire.** The only readers of an orphan row are `get_checkpoint` (`mod.rs:2498`), `release_halt`'s backward check (`mod.rs:2599`), and a same-id re-subscribe's catch-up seed (`mod.rs:3912-3922`). The live listener only seeds ids present in the snapshot (`mod.rs:2065`). Retention + "fresh model ⇒ fresh generation id" is coherent; deletion races the lingering flush (V3). |
| V5 | Retirement **restores `wait_until_all_caught_up` resolvability** for a wedged ReplayAlways Halt subscriber: the gate snapshots `subscriber_modes` (`mod.rs:2971-2978`), and a wedged ReplayAlways Halt id pins its HWM below head forever (no checkpoint row; private fetch excludes ReplayAlways, `mod.rs:2116-2131`; Halt refuses the backstop, `subscriber_state.rs:413-434`). Removing the registry entry drops it from the snapshot. All per-subscriber readiness methods then return `SubscriberNotFound` (`mod.rs:2730-2731`, 2905), symmetric with today's unknown-id behaviour. |
| V6 | Retired ids do **not** fire `RebuildNeededCallback` forever: `scan_late_materialized_gaps` fires the callback once per unresolved row and then auto-resolves it with `resolved_by='gap_detection'` (`mod.rs:447-468`). But the scan does **not** filter by registration (`mod.rs:409-417`), so a retired id yields one zombie callback per row. Recommend resolving at retire with `resolved_by='unsubscribe'`. |
| V7 | **Advisory-lock release is not reliably possible and already broken.** Coordinated subscribe takes a session-scoped `pg_try_advisory_lock` via a pooled connection (`mod.rs:4292-4313`); `release_subscriber_lock` (`mod.rs:3499-3512`) runs on a different pooled session and returns `false` — and has **zero callers** in the crate today. Unsubscribe should attempt release best-effort and document that locks die only with the session/pool, or spec 0031 fixes lock placement. |
| V8 | epoch_mem parity: `unsubscribe` is trivial there (`RwLock<Vec<Box<dyn EventObserver>>>` at `event_store.rs:557`; read lock taken **per event** at 623, so removal under the write lock blocks at most one event's processing including retries) and worth adding for trait parity — but it is a breaking change to `epoch_core::EventBus` (`event_store.rs:231/247`) unless default-implemented. |
| V9 | Unsubscribe-then-resubscribe the same id is a clean new lifecycle (no reuse warning after removal), **but** concurrent with a wake still holding the old `Arc`, first-registration-wins seeding (`mod.rs:2065` `contains_key`, task-input dedup at 2309-2320) feeds events to the **old** observer for exactly one more wake; the new observer receives nothing until the next wake. Window = one wake; document it. |
| V10 | Contract shape: `unsubscribe(subscriber_id) -> Result<bool, PgEventBusError>` — `Ok(true)` removed / `Ok(false)` unknown, idempotent, valid in both dispatch modes. Deliberately **not** `SubscriberNotFound` (unlike readiness): teardown re-runs must not fail, and unsubscribe has no false-ready misread hazard. Retire-by-prefix stays in the consumer layer (catacloud generation chains compose `unsubscribe` + fresh `subscribe`). |

---

## 2. Method

- Read-verification only, at rev `5c27432` (confirmed `git rev-parse`; tree clean).
  Fix commit 2bf9512 confirmed present: `fix(pg): release the projections lock across the listener's batch drain`.
- Every `file:line` in this paper was read in full context; registry mutation sites were
  enumerated by grepping all `projections`/`hwm`/`subscriber_modes` accesses.
- No Postgres probes (per task), no code changes. `git status` shows only this report.

---

## 3. Inventory of subscriber-participating state (scope 1)

### 3.1 In-memory, shared across bus clones

| State | Anchor | What unsubscribe must do | Why |
|---|---|---|---|
| `projections: Arc<Mutex<Vec<Arc<Mutex<dyn EventObserver<D>>>>>>` | `mod.rs:1304` | **Remove** all `Arc`s whose observer id matches (id-read must avoid observer mutexes — V1) | Only mutations today are the two `subscribe()` pushes (`mod.rs:4260` inline, 4732 async); removal is the retire. Removal under the outer mutex is safe: wake snapshots (`1812-1814`, `2038-2041`), inline snapshots (`3642-3644`), and `fast_forward` (`3022`) clone `Arc`s under the lock and drop it before dispatch; nothing holds the outer mutex across `on_event`. |
| `hwm: Arc<Mutex<HashMap<String,u64>>>` | `mod.rs:1420` | **Remove** the entry (ReplayAlways) | Only inserts today (`mod.rs:936-939` live advance, 4342-4344 subscribe-reset; reads at 2077, 3821, 3908). No removal path exists anywhere. A same-id ReplayAlways re-subscribe resets the HWM to 0 anyway (`mod.rs:4341-4346`), so removal vs retention is observationally equivalent today — remove for hygiene and to bound memory. |
| `subscriber_modes: Arc<Mutex<HashMap<String,SubscriptionMode>>>` | `mod.rs:1429` | **Remove** the entry — this is *the* registry operation | Sole source for readiness gating (`subscriber_mode` `mod.rs:2718-2732`; all-gate snapshot `2971-2978`) and for `release_halt`'s mode-resolved WARN (`2643`). Inserted only via `warn_if_subscriber_id_reused` (`3758-3783`) on both subscribe paths (4255 inline, 4329 async). Exists precisely because the observer mutex is held across `on_event` (doc 1424-1437) — the same argument that forces V1. |

### 3.2 Listener-task-local (not per-wake — see V3)

Declared at `mod.rs:1872-1885`, before the reconnect loop (1890); survive reconnects and
wakes; die only when the listener task ends. Reachable only from inside the task closure.

| State | Anchor | What unsubscribe can do about it | Why |
|---|---|---|---|
| `subscriber_states` | 1880 | Nothing (without a refactor); mitigate with a per-wake sweep | Seeded per wake for snapshot ids only (2065 `contains_key`). A lingering entry for a retired id is never driven (task inputs come from `projections_snapshot`, 2294/2336), but (a) a lingering *healthy* entry pins `compute_shared_floor` low (`mod.rs:2208-2212`) → one wasted fetch batch per wake (loop breaks on `!any_subscriber_processed`, 2377-2380); (b) a lingering *wedged* entry enters `wedged_sids` (2129-2138) but the private fetch no-ops on the `sid_to_proj` miss (2141-2144) — safe. |
| `pending_checkpoints` | 1876 | Nothing (without a refactor) | Flushed wholesale by `flush_all_pending_checkpoints` before reconnect (1892-1902) and at shutdown (1965-1975) — **can resurrect a deleted checkpoint row** for a retired id (V3/V4). |
| `checkpoint_cache`, `last_event_ids` | 1872, 1885 | Nothing | Pure caches; inert once the id leaves the snapshot. |

Per-wake locals that ARE rebuilt cleanly each wake: `sid_to_proj`, `replay_always_by_sid`
(`mod.rs:2050-2051`), `wedged_sids` (2140). The next wake after unsubscribe simply never
re-seeds the retired id, because it is gone from the snapshot.

### 3.3 Persisted per-subscriber state

| Table | Schema anchor | What unsubscribe should do | Why |
|---|---|---|---|
| `epoch_event_bus_checkpoints` | m003 `:63-71` + m008 (PK `(bus_name, subscriber_id)`); rename m004 `:121` | **Retain** (see §6). There is no `delete_checkpoint` API today (no `DELETE FROM epoch_event_bus_checkpoints` anywhere). | Position of a durable model; deleting races the lingering flush (V3) and changes re-subscribe semantics (§6). |
| `epoch_event_bus_gap_timeouts` | m009 `:35-48`, unique `(bus_name, subscriber_id, skipped_sequence)` | **Resolve** unresolved rows for the id with `resolved_by='unsubscribe'` (+ note) | Audit preserved; silences the one zombie `RebuildNeededCallback` per row (V6). Reader: `scan_late_materialized_gaps` (`mod.rs:394-470`) — registration-agnostic by design. |
| `epoch_event_bus_dlq` | m003 `:86-99` + m006 resolution columns | **Retain** | Terminal audit record; m003's own comment ("DLQ entries serve as audit records") argues retention. Nothing re-drives DLQ rows automatically; management APIs are per-subscriber and keep working for retired ids: `get_dlq_entries` (`mod.rs:3080`), `get_dlq_entries_paginated` (3127), `count_dlq_entries` (3172), `remove_dlq_entry` (3200), `remove_all_dlq_entries` (3227). |

### 3.4 Advisory locks

`try_acquire_subscriber_lock` (`mod.rs:3477-3496`), used by Coordinated subscribe
(`4292-4313`); `release_subscriber_lock` (`3499-3512`) exists but has **no callers**.
Both run through `&pool`, i.e. an arbitrary pooled session. `pg_try_advisory_lock` /
`pg_advisory_unlock` are **session-scoped**: the lock outlives the query on whatever
pooled session happened to serve it, and an unlock from a different session returns
`false`. So "unsubscribe releases the lock" is not implementable reliably with the
current shape (V7). Unsubscribe must do: best-effort `release_subscriber_lock`
(documented as unreliable), or spec 0031 re-plans lock ownership (see OQ-4).

---

## 4. The unsubscribe/retire contract (scope 2)

### 4.1 Signature options

1. **Exact-id `unsubscribe(&self, subscriber_id) -> Result<bool, PgEventBusError>`** (recommended).
   `Ok(true)` = removed; `Ok(false)` = id not registered (idempotent no-op).
   Contrast with readiness, whose unknown-id answer is `SubscriberNotFound`
   (`mod.rs:1231`, doc 1225-1230; raised at 2730-2731 in `subscriber_mode` and 2905 in
   `wait_until_caught_up`): there the error exists because a silent fallback would
   *false-ready* a ReplayAlways subscriber. Unsubscribe has no analogous misread, and a
   teardown path (drop handler, operator script, generation roll) that re-runs should not
   start failing. Returning `bool` keeps observability (caller can warn on `false`).
2. **`Err(SubscriberNotFound)` variant** — maximally symmetric with readiness, but makes
   retire non-idempotent. Acceptable only if the caller contract is "operator asserts the
   id exists"; harder to compose in generation rolls.
3. **Retire-by-prefix** (`retire_matching("<base>#")`): not recommended in-framework.
   The id is opaque to epoch; prefix matching invites mass-removal races against a
   concurrent `subscribe()` of the next generation, and the catacloud
   `<base>#genN` scheme (CLOUD-231/CLOUD-259 lineage) composes cleanly from exact-id
   unsubscribe + fresh subscribe at the consumer layer. What the framework *must*
   guarantee is that unsubscribe makes the id fully re-registrable (§8), because
   first-registration-wins currently makes same-id re-subscribe a silent zombie
   (`warn_if_subscriber_id_reused`, `mod.rs:3758-3783`).
4. **Dispatch-mode scope**: unsubscribe should work on `DispatchMode::Inline` buses too —
   the inline path pushes observers (`mod.rs:4260`) and its drain snapshots per queue
   entry (`3642-3644`), so removal takes effect on the next drained event. No
   `InlineDispatchNotSupported` analogue needed (readiness rejects Inline because there
   is no position to poll, `2712-2716`; removal has no such dependency).

### 4.2 Idempotency / unknown id

Covered above. One extra asymmetry to document: between "removed from
`subscriber_modes`" and "removed from `projections`", a readiness call sees
`SubscriberNotFound` while the listener may still be driving the observer (V2 window).
If the two removals happen under one sequential `unsubscribe` body this interleave is
microseconds-wide and only observable to a concurrent readiness poll; document, don't
engineer around.

### 4.3 Effect timing given snapshot-then-release (2bf9512)

The select loop snapshots and releases (`mod.rs:2036-2041`), with the comment noting a
mid-drain `subscribe()` lands "on the next wake". The symmetric statement for retire:

- The current wake holds `projections_snapshot` Arcs (2051) and re-iterates them **for
  every batch of that wake** (2294), clones per task input (2336), and
  `process_event_with_retry` takes `&Arc<Mutex<dyn EventObserver>>` (`retry.rs:142-152`)
  — the `Arc` clone count keeps the retired observer alive and **driven until the wake
  ends**. Under an active backlog that is the whole remaining drain, not one batch.
- That wake keeps flushing the retired id's checkpoints: per-batch outcome merge
  (2299-2307), per-tick `flush_expired_checkpoints` (1960-1963), pre-reconnect and
  shutdown `flush_all_pending_checkpoints` (1892-1902, 1965-1975).
- **Bound**: end of the current wake's batch loop (break on empty rows / non-full batch /
  no processed events, 2377-2380). No lock fences it: unsubscribe acquires the outer
  `projections` mutex only between wakes' snapshot acquisitions, and the in-flight wake
  already dropped it.
- Practical consequence for a Checkpointed retiree: its checkpoint row may advance past
  the unsubscribe point during the tail wake. Combined with row retention (V4) this is
  harmless for a *discarded* model; it narrows the same-id-re-subscribe race in §8 to
  "catch-up seed may be slightly newer than expected", which is at-least-once-safe.

### 4.4 In-flight `on_event` interaction

- `invoke_observer_once` (`retry.rs:43-60`) takes the observer's inner mutex **before**
  awaiting `on_event` and holds it across the call (including the retry ladder in
  `process_event_with_retry`, `retry.rs:142-152`, which holds `&Arc` throughout).
- Therefore a retire implementation that reads `subscriber_id` by locking each observer
  (the only identification mechanism today: `fast_forward_all_subscribers`
  `mod.rs:3026-3035`, R2 pass 1820-1827, wake init 2055-2064) **blocks unboundedly**
  behind a wedged handler — violating the ticket's "safe on wedged subscribers".
- The fix is structural: promote the registry from `HashMap<String, SubscriptionMode>`
  to one that also carries the `Arc` (e.g. `HashMap<String, (SubscriptionMode,
  Arc<Mutex<dyn EventObserver<D>>>)>`), written by `subscribe()` and read by
  `unsubscribe()` without touching observer mutexes; removal from the `Vec` then matches
  by `Arc::ptr_eq` under the outer mutex. This simultaneously fixes
  `fast_forward_all_subscribers`' pre-existing wedge-blocking scan.
- Same-id duplicates in the `Vec` (first-registration-wins leaves both, `mod.rs:2309-2320`
  dedup + 2065 seeding): unsubscribe removes **all** matching Arcs; only the first is
  ever driven, the second is a pure zombie.

---

## 5. Effect on every consumer of the registry (scope 3)

| Consumer | Anchor | Post-retire behaviour | Verdict |
|---|---|---|---|
| `subscriber_mode` (gate) | `mod.rs:2718-2732` | `SubscriberNotFound` | This is the switch: removing the entry is what makes every readiness API treat the id as gone, and what drops it from the all-gate snapshot. |
| `wait_until_caught_up` | `mod.rs:2898-2925` | `SubscriberNotFound` via 2905, even on an empty bus | Symmetric with today's unknown-id behaviour; callers must treat it as terminal, not transient. Document. |
| `wait_until_all_caught_up` | `mod.rs:2952-2978` | Retired id excluded from the snapshot (2971-2978) → gate can resolve | For a wedged ReplayAlways Halt id: HWM pinned below head (no checkpoint row, `mod.rs:936-938`; ReplayAlways excluded from the private wedge fetch, 2116-2131; Halt refuses the backstop, `subscriber_state.rs:413-434`), so today the gate returns `Ok(false)` forever. Retirement is the remedy. Hazard to document: retiring the **last** subscriber makes the gate trivially `Ok(true)` (2958-2963). |
| `subscriber_lag` | `mod.rs:2804-2843` | `SubscriberNotFound` (validate-first at 2810-2812) | Monitoring must stop watching the id; a lag dashboard that errors per retired id should treat `SubscriberNotFound` as "retired". |
| `fast_forward_all_subscribers` | `mod.rs:2999-3064` | Retired id not in the snapshot (3022) → no row write | Desired. Pre-existing hazard stays: this method locks each observer's inner mutex (3027), so a wedged live observer blocks it — the §4.4 registry redesign fixes this too. ReplayAlways already skipped (R5, 3026-3035). |
| `release_halt` | `mod.rs:2594-2686` | **Unchanged, still succeeds** for a retired id | `release_halt` deliberately writes checkpoint rows for *never-registered* ids too (2625-2633); retire adds no new inconsistency. But note: a release written after retire seeds a future same-id re-subscribe's catch-up (`3912-3922`) — i.e. release-then-resubscribe suppresses replay up to the released position. Flag in docs; see OQ-3 for whether unknown/retired ids should error. |
| `check_skipped_gaps` / periodic scan | `mod.rs:3424-3444`, `394-470`; scan task 2389-2410 | Rows for retired ids still detected; callback fires **once** per row, then row auto-resolves with `'gap_detection'` (447-468) | Not "forever" (V6), but each firing is a zombie notification: the callback's remedy ("drop the model and re-subscribe", config.rs `RebuildNeededCallback` docs 268-297) targets a model that no longer exists. Recommended policy: at retire, `UPDATE ... SET resolved_at=NOW(), resolved_by='unsubscribe'` for unresolved rows of `(bus_name, subscriber_id)` — audit intact, callbacks silenced. Alternative (leave rows) is bounded noise: one callback per row, at-least-once under crash races (doc 296-299). |
| `resolve_gap_timeout` | `mod.rs:3446-3475` | Works for any id | Retirement policy can reuse it (with `resolved_by='unsubscribe'`). |
| `try_acquire/release_subscriber_lock` | `mod.rs:3477-3512` | Release best-effort, unreliable (V7) | Session-scoped locks on pooled sessions; `release_subscriber_lock` from another session returns `false`; zero callers today. |
| DLQ readers/cleaners | `mod.rs:3047-3252` | Unaffected | Per-subscriber APIs keep operating on retained rows. |
| Inline drain | `mod.rs:3618-3660` | Retired observer absent from the next per-entry snapshot | Removal effective on the next drained event. |

---

## 6. Checkpoint rows: delete vs retain for a retired `Checkpointed` subscriber (scope 4)

**Recommendation: retain.** Reasoning:

- **Who reads an orphan row?** Exactly three readers, none automatic:
  1. `get_checkpoint` (`mod.rs:2498-2510`) — public diagnostic;
  2. `release_halt`'s backward-release check (`mod.rs:2599`) — only if someone releases a
     retired id, which today is *allowed by design* (§5);
  3. `catch_up_from_checkpoint`'s seed (`mod.rs:3912-3922`) — only on a same-id
     re-subscribe.
  The live listener reads checkpoint rows only for ids in the current snapshot
  (`mod.rs:2085-2101`); a retired id is never re-seeded, so orphan rows are inert while
  retired. `compute_shared_floor` and the gap scan never read the checkpoints table.
- **Deletion races the lingering flush (V3).** Delete-then-listener-keeps-running has a
  window (until wake end + flush tick, and worst-case the shutdown/reconnect
  `flush_all_pending_checkpoints`) in which the row is re-created by the blind upsert
  (`flush_checkpoint`; `update_checkpoint` doc at `mod.rs:2526-2540` explicitly calls it
  non-monotonic). Full closure would require moving the four maps into shared state — a
  refactor spec 0031 may not need.
- **Semantics:** retention makes "same-id re-subscribe after unsubscribe" a **resume**
  (catch-up from the retained row), matching the Checkpointed model's durability story:
  the row records what the model with *that id* has applied. A **rebuilt** model is a new
  model and must take a new generation id — exactly the catacloud `<base>#genN` scheme.
  This mirrors the existing ReplayAlways rule (fresh subscribe resets the HWM to 0,
  `mod.rs:4339-4346`, because the in-memory model is rebuilt from empty): for
  Checkpointed, "fresh model" is expressed by the id, not by wiping the row.
- If a purge is ever wanted, it should be an explicit operator action (a
  `delete_checkpoint(subscriber_id)` public API, which does not exist today) run after
  the bus is quiescent — not a side effect of unsubscribe.

---

## 7. epoch_mem parity (scope 5)

`epoch_mem/src/event_store.rs`:

- `projections: Arc<RwLock<Vec<Box<dyn EventObserver<D>>>>>` (`:557`); in-memory DLQ
  `:560`; the background loop takes the **read** lock per event and iterates (`:623`,
  comment `:618-622`: held across the full observer loop including retry sleeps);
  `subscribe` (`:755-768`) pushes a `Box` under the write lock. **No unsubscribe.**
- Shape differences that make parity *easier* here:
  - `Box`, not `Arc` — no snapshot-clone problem; removal under the write lock is a plain
    `Vec::retain`/`swap_remove`.
  - The read lock is taken **per event**, so an unsubscribe blocks at most one event's
    processing (including retries), not an unbounded backlog drain. The epoch_pg wedge
    hazard (V1/V2) has no epoch_mem analogue — there is no per-observer inner mutex at
    all (sequential single-consumer loop), and no persisted state to resolve (no
    checkpoints table, in-memory DLQ only, no advisory locks).
  - A hanging handler still wedges the whole epoch_mem loop, and an unsubscribe would
    wait behind it — same *observable* limitation as V1 but with a different root
    (single consumer task, no registry).
- **Recommendation: add `unsubscribe` to epoch_mem**, for two reasons:
  1. Consumers writing mode-agnostic code (and catacloud-style generation rolls) need the
     same entry point on both buses; divergent APIs would force `cfg`/trait-object forks.
  2. It is cheap: ~10 lines, no new state.
  The cost is API surface: `unsubscribe` must appear on the `EventBus` trait
  (`epoch_core/src/event_store.rs:231`, `subscribe` at 247). Options: (a) breaking trait
  change, (b) defaulted trait method (e.g. default `Ok(false)` / `Err(Unsupported)`).
  See OQ-5. If the framework instead documents "retire is epoch_pg-only", epoch_mem
  subscriptions remain process-lifetime — defensible for a test double, but the ticket's
  generation-chain consumer story would not be portable to in-memory test rigs.

---

## 8. Interaction with first-registration-wins (CLOUD-231) (scope 6)

- Today: `warn_if_subscriber_id_reused` (`mod.rs:3758-3783`) inserts the new mode and
  warns if present. The second `subscribe()` of a **live** id runs full catch-up
  (advancing the *shared* checkpoint/HWM!) and is then pushed — but per-wake seeding
  (`2065` `contains_key`) and task-input dedup (`2309-2320`) keep the **first** observer,
  so the second receives no live events. That is CLOUD-231's trap, unchanged by this
  paper.
- With unsubscribe: removing the registry entry makes the next `subscribe()` of that id a
  clean registration (no warning, no zombie predecessor *in the registry*).
- **Same-id re-subscribe while a wake is in flight** (the retiree's Arc still in the
  snapshot, V2): the wake's seeding is id-keyed, so the **old** observer keeps being
  driven and the new observer receives nothing until the next wake re-seeds from a
  snapshot that contains only the new Arc. Window = exactly one wake. Meanwhile both
  observers, if the old one is Checkpointed, interleave writes to the same checkpoint row
  (last-writer-wins, non-deterministic but at-least-once-safe). Document: "after
  unsubscribe, re-subscribe of the same id may be inert for up to one wake".
- **ReplayAlways re-subscribe**: HWM reset to 0 (`4341-4346`) — replay from 0. Combined
  with spec 0030 §4 (`specs/0030-cloud261-sequence-burn-resilience.md:101`, verified:
  no separate ticket exists) — *a fresh subscribe over any hole double-delivers the
  sequences just above it in the catch-up pass (`processed_ahead` guards the live path
  only)* — any auto-heal that re-subscribes over a hole (P5, fresh generation id) hits
  that bug on every heal. This paper does not re-probe it (no-probe mandate); it is
  restated as a hard prerequisite for spec 0031's P5 part.

---

## 9. Premises confirmed / overturned

| Premise (from brief / CLOUD-231 / spec 0030) | Verdict |
|---|---|
| "No `unsubscribe()` anywhere in the crate" (CLOUD-231) | **Confirmed** — no `fn unsubscribe`; `projections` mutation sites are only the two pushes (4260, 4732); `subscriber_modes`/`hwm` have no removals. |
| Per-wake locals are "rebuilt each wake, removal between wakes is clean" (brief) | **Overturned (refined)**: `sid_to_proj`/`replay_always_by_sid`/`wedged_sids` are per-wake (2050-2051, 2140), but `subscriber_states`/`pending_checkpoints`/`checkpoint_cache`/`last_event_ids` are listener-task-lifetime (1872-1885, outside the reconnect loop at 1890) and are flushed wholesale on reconnect/shutdown (1892-1902, 1965-1975). Unsubscribe cannot clean them without a refactor. |
| Snapshot-then-release makes unsubscribe "acquire the lock quickly even during a drain" (brief, 2bf9512) | **Confirmed** — and sharpened: lock acquisition is quick, but the *effect* stays bounded at wake end, which under a long drain is the whole remaining backlog (§4.3). |
| A wedged ReplayAlways Halt subscriber pins `wait_until_all_caught_up` (brief, "to be probed") | **Confirmed by code reading** (mechanism at §5, V5): HWM pinned, no private fetch, backstop refused; registry snapshot excludes retired ids, so retirement restores resolvability. Not live-probed (no-probe mandate). |
| Unresolved gap-ledger rows would fire `RebuildNeededCallback` forever (task question) | **Overturned**: one firing per row, then auto-resolve `'gap_detection'` (447-468); the residual issue is the zombie callback for a retired id (V6). |
| `release_halt` deliberately writes rows for unregistered ids (brief) | **Confirmed** at 2625-2633, with honest per-mode WARN since spec 0030 R8 (2632-2653). |

---

## 10. Implications for spec 0031

1. The retire API's hard requirement is not the removal itself (trivial) but
   **identification without observer mutexes** (V1) — this pulls a small registry
   redesign into scope and retrofits `fast_forward_all_subscribers`.
2. "Effect at next wake" must be a documented semantic, not a bug: tests and consumers
   should expect up to one wake of post-retire delivery + checkpoint advances (V2/V9).
3. Retire must *not* delete checkpoint rows (V4); spec 0031 should state the
   generation-chain convention (fresh model ⇒ fresh id) explicitly, since it is what
   makes retention coherent.
4. The gap-ledger resolve-at-retire policy (V6) is one SQL statement; cheap to include,
   and it prevents zombie `RebuildNeededCallback`s from confusing rebuild automation.
5. Advisory locks: either spec 0031 accepts best-effort release with documentation (V7),
   or it fixes lock ownership (dedicated connection per subscription). Fixing is a
   behaviour change for Coordinated mode — flag as its own decision (OQ-4).
6. P5 (auto re-subscribe with fresh generation) depends on: same-id-trap avoidance
   (unsubscribe), and the spec 0030 §4 catch-up double-delivery bug being fixed or
   dodged — otherwise every heal over a hole double-delivers.
7. epoch_mem parity is cheap and keeps the consumer story portable (V8); the trait
   question (OQ-5) is the only real cost.

---

## 11. Recommended minimal contract for spec 0031 (numbered)

1. **API**: `unsubscribe(&self, subscriber_id: &str) -> Result<bool, PgEventBusError>`;
   `Ok(true)` removed, `Ok(false)` unknown; idempotent; valid on Async **and** Inline
   buses; never acquires an observer's inner mutex.
2. **Registry redesign (prerequisite)**: extend the per-bus registry to map
   `subscriber_id -> (mode, Arc<Mutex<dyn EventObserver<D>>>)` (replacing or augmenting
   `subscriber_modes`), so unsubscribe and `fast_forward_all_subscribers` identify
   observers without locking them; Vec removal by `Arc::ptr_eq` under the outer
   `projections` mutex; remove **all** same-id Arcs.
3. **Removal procedure, in order**: (a) registry entry out (readiness flips to
   `SubscriberNotFound` from here); (b) Vec Arcs out; (c) `hwm` entry out (ReplayAlways);
   (d) unresolved `epoch_event_bus_gap_timeouts` rows for `(bus_name, subscriber_id)`
   resolved with `resolved_by='unsubscribe'`; (e) Coordinated mode: best-effort
   `release_subscriber_lock`, documented as cross-session-unreliable; (f) checkpoint row
   and DLQ rows **retained**.
4. **Timing contract**: removal takes effect at the next wake; the current wake may
   still deliver events to, and advance checkpoints of, the retired observer (bounded by
   wake end; unbounded under an active drain). Document the one-wake inertness of a
   same-id re-subscribe (§8).
5. **Listener-state hygiene (minimal)**: at each wake's init pass, drop
   `subscriber_states`/`pending_checkpoints`/`checkpoint_cache`/`last_event_ids` entries
   whose ids are absent from the new snapshot (a one-line `retain` per map at
   `mod.rs:~2050`). This bounds floor-pinning and mostly closes the
   shutdown-flush-resurrection window without moving the maps into shared state.
6. **Re-subscribe semantics**: same-id re-subscribe after unsubscribe = new lifecycle;
   Checkpointed resumes from the retained row, ReplayAlways replays from 0 (existing HWM
   reset). Fresh models take fresh generation ids; the live-id double-subscribe warning
   (CLOUD-231) stays as-is.
7. **epoch_mem parity**: add `unsubscribe` with identical signature; removal under the
   write lock; no persisted-state steps. Ship via a defaulted `EventBus` trait method or
   a breaking trait bump (OQ-5).
8. **Docs**: `release_halt` for a retired id still writes rows (documented behaviour,
   unchanged); `wait_until_all_caught_up` trivially `Ok(true)` on an empty registry
   (existing hazard, now reachable via retire).

---

## 12. Open questions for Ruben

1. **Error semantics**: `Ok(false)` idempotent unsubscribe (recommended) vs
   `SubscriberNotFound` for symmetry with readiness?
2. **Checkpoint retention**: confirm retain-and-resume + "fresh model ⇒ fresh generation
   id" as the framework's stated convention (recommended), vs delete-at-retire with
   explicit `delete_checkpoint` as an operator action?
3. **`release_halt` on retired/unknown ids**: keep the documented write-for-anyone
   behaviour (2625-2633), or harden to `SubscriberNotFound` (behaviour change)?
4. **Advisory locks**: is fixing cross-session unlock (dedicated lock connection per
   subscription) in scope for 0031, or defer with documented best-effort release?
5. **`epoch_core::EventBus` trait**: breaking `unsubscribe` addition vs defaulted method,
   given epoch_mem must match?
6. **Registry redesign scope**: fold the id→Arc registry (which also unblocks
   `fast_forward_all_subscribers` behind wedges) into 0031, or land retire on the
   current Vec-scan and accept wedge-blocking unsubscribe?
7. **P5 sequencing**: confirm the spec 0030 §4 catch-up double-delivery bug must be
   fixed (or dodged by replay-from-0-only heals) before any auto-resubscribe ships.
