# 0031 Implementation Plan — TDD (post-approval execution)

Source of truth: `specs/0031-cloud262-observer-retire-p5-wedge-heal.md` (approved
2026-09-17). This plan turns the spec's §9 delivery plan into ordered TDD cycles.
Baseline rev `5c27432`, branch `cloud-262`, worktree
`/root/code/epoch-worktrees/cloud-262`.

## Ground rules (apply to every phase)

1. **TDD order**: failing test first (compile-fail counts for new APIs), then the
   minimum implementation to green, then refactor. Tests are committed with the code
   that makes them pass (never land a red test).
2. **Anchor freshness**: spec line anchors are from `5c27432`. Before each edit, re-derive
   the site by symbol name (`grep -n "fn catch_up_from_checkpoint"` etc.); line numbers
   shift as earlier phases land. If a re-derived anchor contradicts the spec's claim,
   stop and reconcile before editing.
3. **Test environment**: docker rig `cloud-259-postgres-1` on `127.0.0.1:5457`
   (`PGPASSWORD=postgres`). Never touch `catacloud*`, `test_*`, or `epoch_pg_test*`
   databases. Per-phase scratch DB (isolated, disposable):

   ```text
   docker ps | grep cloud-259-postgres-1   # rig must be up
   docker exec cloud-259-postgres-1 psql -U postgres \
     -c 'DROP DATABASE IF EXISTS epoch_0031;' -c 'CREATE DATABASE epoch_0031;'
   export DATABASE_URL=postgres://postgres:postgres@localhost:5457/epoch_0031
   export EPOCH_REQUIRE_DB=1
   ```

4. **Validation gate** (per phase, before commit):
   - `cargo fmt --all`
   - `cargo clippy --workspace --all-targets -- -D warnings` (zero warnings)
   - `cargo test -p epoch_pg` (DB-gated, env above) — all new tests + full suite green
   - `cargo test --workspace` (epoch_core / epoch_mem)
   - Integration tests are `#[serial]`, use `isolated_events_table` (mod.rs pattern at
     `:5747`), relative sequence assertions only — repeatable across runs.
5. **No new dependencies.** No `unwrap()`/`expect()` outside tests. Rustdoc on every
   new public API. Conventional Commits (`feat|fix|refactor|test|docs(pg|core|mem|event-bus|...)`).
6. **One writer**: all edits in this worktree, sequential phases. Spec allows 1 ∥ 2;
   taking the spec's safe default (1 → 2 → 3) since both touch `mod.rs`.
7. Per-phase scope fences from the spec's "Explicitly out of bounds" bullets are hard
   stops — anything that seems to need them goes back to the user, not into the diff.

## Phase 0 — Land the docs (5 min)

The six untracked files (4 probe reports, discovery, spec) must be committed before
implementation so code commits stay separable.

- `git add docs/probes/cloud262-*.md specs/0031-cloud262-*.md`
- Commit: `docs: add CLOUD-262 spec 0031, discovery, and probe reports`

## Phase 1 — Exactly-once fresh subscribe over open holes (R1–R3)

**Goal**: fresh `ReplayAlways` subscribe over a burned hole delivers above-hole rows
exactly once (counts `(3,1),(4,1)` — today `(3,2),(4,2)`), position pinned at the
hole, across boots and listener restarts.

**Cycle 1.1 — failing repro test** (`epoch_pg/tests/pgeventbus_integration_tests.rs`):

- `test_fresh_subscribe_over_open_hole_delivers_exactly_once`
  Recipe (probe report 4 B1): `isolated_events_table`, `snapshot_fencing: false`,
  tiny `gap_timeout`; burn seq 2 via claim-in-open-tx-then-rollback; publish 3, 4;
  fresh `ReplayAlways` subscribe; assert delivered = `(3,1),(4,1)` (relative seqs) and
  position pinned below the hole. Run → **FAIL** (double delivery).
- Implementation to green:
  - `SubscriberState::new_with_event_id` (subscriber_state.rs, ~170-186) accepts the
    exact delivered set. **Decision: `HashSet<u64>`** (`delivered_above_prefix`) —
    exact, no coalescing logic to get wrong; bounded by one catch-up's visible rows.
    Never an upper bound alone (late-materialization repro in spec §9 Phase 1).
  - `catch_up_from_checkpoint` (mod.rs ~3892-4163): return the exact delivered
    sequences alongside the existing `(u64, u64)` (small struct, e.g.
    `CatchUpOutcome { below, above, delivered: HashSet<u64> }`). **Do not change what
    it delivers.**
  - Carrier: shared map beside `hwm`/`subscriber_modes`, e.g.
    `pending_delivered_sets: Arc<Mutex<HashMap<String, Vec<u64>>>>`; subscribe's
    catch-up call (~4439-4448), the buffer-drain leg (~4477-4505), and the startup
    replay pass call site (~1846-1856, R2) each record the set.
  - Listener init pass (~2063-2111): consume the set when constructing
    `SubscriberState` for a fresh id.
  - Dedup consumption point (mod.rs ~527-533): consult the set beside
    `processed_ahead.contains`.
  - Amend the hole-blind CLOUD-225 comment (~2032-2039) to state the true behaviour.

**Cycle 1.2 — remaining pins** (each red→green against the cycle 1.1 code):

- `test_listener_restart_over_open_burn_live_pass_adds_no_copies` (R2 pass leg)
- `test_fresh_subscribe_hole_free_stream_unchanged`
- `test_fresh_subscribe_position_still_pins_at_hole` (R3)
- `test_fresh_subscribe_late_materialized_row_inside_catchup_range_delivered_once`
  (the test that kills the rejected range-watermark shape: hold a mid-range seq in an
  open txn, burn the hole, publish above, subscribe, commit the held row → exactly once)
- `Checkpointed`-variant confirm leg (report 4 OQ-2): checkpointed fresh subscribe
  over the same hole resumes from checkpoint, unchanged behaviour.

**Out of bounds**: no change to what catch-up delivers; no `compute_shared_floor`
(~2206-2211) or fetch cursor changes; no resolver changes; no migration; no retire/heal.

**Exit**: 6 new tests green; CLOUD-261/0030 Part A suite green (incl. `Halt` default
pins); gate clean.
**Commit**: `fix(pg): fresh subscribe over open holes delivers above-hole rows exactly once`

## Phase 2 — Lock-free id→observer registry (R5)

**Goal**: identify any observer by id without acquiring its mutex;
`fast_forward_all_subscribers` completes while a wedged handler holds its mutex.

**Cycle 2.1 — failing test**:

- `test_fast_forward_completes_while_observer_mutex_held` — observer whose `on_event`
  parks on a channel; call `fast_forward_all_subscribers` and assert it returns within
  a bounded timeout. Run → **FAIL** (today it deadlocks on the parked mutex).
- Registry-shape unit test (value carries mode + all same-id Arcs).

**Cycle 2.2 — implementation**:

- Extend `subscriber_modes` (mod.rs ~1429 + rustdoc ~1416-1428) to
  `HashMap<String, (SubscriptionMode, Vec<Arc<Mutex<dyn EventObserver<D>>>>)>`.
  (Single-Arc value rejected in review: delivery is first-registration-wins via
  `seen_sids` (~2314-2321), so overwriting would retain the inert duplicate, not the
  delivering observer — R6 removal would be unachievable.)
- Retrofit all readers/writers: push sites ~4259-4260 (inline) and ~4731-4732 (async)
  **append**; `warn_if_subscriber_id_reused` (~3758-3775) pushes onto the existing
  `Vec` while still warning; `subscriber_mode` (~2718-2732), readiness snapshot
  (~2966-2972), `release_halt` mode lookup (~2640-2645) read the entry's mode.
- `fast_forward_all_subscribers` (~2999-3041): resolve mode/id from the registry;
  never lock observers to read metadata; `projections` snapshot stays for the update
  path only.
- Add the capture-based `Arc::ptr_eq` removal helper (internal fn near the registry):
  takes an already-captured `Vec<Arc<...>>`, removes every captured Arc from the
  projections Vec by `ptr_eq` under the outer mutex; does **not** look up the registry
  or drop the entry — caller's job, in that order (capture, then drop). Exercised by
  Phase 3.

**Out of bounds**: no public `unsubscribe` yet; no listener-map/tombstone changes;
readiness semantics unchanged (behaviour-preserving phase).

**Exit**: new tests green; full suite green; the six `projections.lock()` sites
(~1813, 2039, 3022, 3646, 4259, 4731 — re-derived) unchanged in count — no silent
new write path.
**Commit**: `refactor(pg): lock-free id-to-observer registry, no observer locking for metadata`

## Phase 3 — `unsubscribe` retire API with full removal inventory (R4, R6–R10)

**Goal**: `unsubscribe(id) -> Result<bool, PgEventBusError>` retires a subscriber
end-to-end on a live bus; tombstone makes removal cross-task-safe; races settled.

**Cycle 3.A — R4 basics** (tests first; method doesn't exist → compile-fail red):

- `test_unsubscribe_removes_registered_subscriber_returns_true`
- `test_unsubscribe_unknown_id_is_idempotent` (`Ok(false)`; idempotent `Ok(bool)`)
- `test_unsubscribe_inline_bus` (Inline: no listener, stops after step (g), no marker)
- Implement: `pub async fn unsubscribe(&self, subscriber_id: &str)` on `PgEventBus`,
  removal order (capture before drop — the Phase 2 helper cannot find Arcs after the
  entry is gone): (a) capture id's `Vec<Arc>` from the registry under the outer
  mutex; (b) remove all captured Arcs from projections via `ptr_eq` helper; (c) drop
  registry entry (readiness flips here); (d) `hwm` entry out; (e) unresolved
  `epoch_event_bus_gap_timeouts` rows → `resolved_by='unsubscribe'` (one UPDATE,
  mirroring `resolve_gap_timeout`); (f) Coordinated: best-effort
  `release_subscriber_lock`, unreliability documented; (g) checkpoint/DLQ rows
  retained (no DELETE); (h) insert tombstone marker last, **only when a listener is
  running**.

**Cycle 3.B — R6/R7 + gate** (tests first):

- `test_retired_id_readiness_returns_subscriber_not_found`
- `test_retiring_lagging_subscriber_does_not_disrupt_peer_and_flips_readiness`
- `test_unsubscribe_wedged_subscriber_restores_all_caught_up_gate`
  (wedged `ReplayAlways`+`Halt` → gate false 3 rounds → unsubscribe → resolves)
- Implement: green via (c) + registry-based readiness (Phase 2 carries this).

**Cycle 3.C — R8 ledger/retention** (tests first):

- `test_unsubscribe_resolves_gap_ledger_rows_with_unsubscribe_marker`
- `test_unsubscribe_retains_checkpoint_row_and_resubscribe_resumes`
- Implement: the (e) UPDATE; retention is "no code added" + test pins it.

**Cycle 3.D — tombstone, fence, races (R9 timing)** (tests first):

- `test_unsubscribe_effect_at_next_wake` (current wake may still deliver; next wake
  does not — the R9 pin)
- `test_unsubscribe_coordinated_best_effort_lock_release` (R10)
- `test_resubscribe_same_id_before_next_wake_starts_clean` (unsubscribe then immediate
  re-subscribe; `ReplayAlways` replays from 0 / `Checkpointed` resumes from the
  retained row; no inherited halt/gap; **determinism**: publish nothing between the
  two calls; assertions tolerate a wake landing in the window or drive one explicitly —
  listener wakes on NOTIFY and the ~1 s `flush_interval` timer)
- Implement (the delicate core, in this order):
  1. Tombstone `retired_ids: Arc<Mutex<HashSet<String>>>` beside the registry;
     `unsubscribe` step (h) inserts only when a listener is running.
  2. Wake init pass, **before** the `contains_key` gate: for each marked id — prune
     the four listener-lifetime maps **and** re-remove `hwm[id]` (the ReplayAlways
     advance path can resurrect it); **fence the id's pre-unsubscribe snapshotted
     Arc from this wake's dispatch** (snapshot predates the unsubscribe, so its Arc
     is necessarily the retired one; a re-registered replacement joins next wake);
     then drop the marker (consumed, not standing).
  3. `contains_key` gate then re-seeds iff the registry shows the id registered again;
     otherwise the id stays pruned. `subscribe()` never touches `retired_ids`.
  4. Subscribe/retire race invariant — **an observer Arc in the projections Vec always
     has a live registry entry**: each push site re-verifies in the same outer-mutex
     critical section (or immediate re-check) that the id is still registered after
     pushing; if a concurrent unsubscribe retired it, remove the just-pushed Arc and
     return `Err(SubscriberNotFound)` (existing variant, non-breaking; rustdoc:
     re-issue the subscribe if still wanted).
  - Why the fence is load-bearing (assert via the battery): without it, a pruned
    non-re-registered id would hit the `SubscriberState::new(0)` fallback
    (~2328-2330, normally unreachable per comment ~2323-2327) → silent FailOpen+Halt
    substitution for a FailClosed subscriber; a re-registered id's stale Arc would
    burn the new lifecycle's replay-from-0 (subscribe zeroes the HWM ~4343-4345).

**Out of bounds**: no heal/P5 logic; no `release_halt` changes; no checkpoint deletion;
no retire-by-prefix; no epoch_mem work.

**Exit**: 11-test battery green; full suite green (readiness/P4b/gap-scan unaffected
for non-retired ids); rustdoc on `unsubscribe` carries timing, retention, and
empty-registry-hazard contracts.
**Commit**: `feat(pg): add unsubscribe retire API with full removal inventory`

## Phase 4 — P5 wedge heal: retire-and-notify with fresh generations (R12–R15)

**Goal**: `ReplayAlways`+`FailClosed`+`Halt` wedged by `GapUnproven` → retired exactly
once per (generation, gap), app callback invoked with fresh `{base}#genN` id under
configured backoff; healed replay delivers exactly once; `SkipAfterBackstop` and
deserialize/observer wedges never heal; cap retires the family with an ERROR and stops.

**Cycle 4.1 — config surface** (tests first):

- `test_unconfigured_bus_never_heals` (default-on guard: both fields default `None`)
- `test_wedge_heal_fires_once_per_generation_on_gap_unproven`
- Implement: `config.rs` — `on_wedge_retired: Option<Arc<dyn WedgeRetiredCallback>>`
  (docs modelled on `RebuildNeededCallback`'s "Epoch provides the trigger, not the
  rebuild", quote at `config.rs:247`); `wedge_heal: Option<WedgeHealPolicy>` with
  `max_generations: u32` + config-injectable backoff (default: immediate boot-
  generation heal; 30 s then 60 s cap on heal-generation re-halts). Optionally add
  `HaltReason::WedgeSuperseded` to the `#[non_exhaustive]` enum only if a phase test
  needs to distinguish it — decide at implementation, default to NOT adding.

**Cycle 4.2 — fire-site gate** (tests first):

- `test_wedge_heal_never_fires_for_skip_after_backstop`
- `test_wedge_heal_never_fires_for_deser_wedge`
- Implement: fire site (`fire_on_halt(GapUnproven)`, inside
  `process_subscriber_for_batch`, ~766-774) only **gate-checks and emits** — no
  unsubscribe/mint/callback work, no new observer locking. Gate resolves from
  `SubscriberState` fields already in scope (`failure_mode`, `gap_policy` — the same
  fields read at ~709-710; `replay_always` reuses the wake's pre-existing
  `subscription_mode()` read at ~497-498). On match with a callback configured: send
  `HealRequest { subscriber_id, held_below_sequence }` over the new
  `Option<mpsc::UnboundedSender<HealRequest>>` in `BatchContext` (~70-85) — `None`
  when unconfigured, so the default path costs nothing. Once-per-(generation, gap) is
  already guaranteed upstream (`halt_fired` set on the false→true transition,
  subscriber_state.rs ~433-435); the fire site must not set it.

**Cycle 4.3 — heal actor + lifecycle** (tests first):

- `test_heal_allocates_fresh_generation_ids` (`#gen2`, `#gen10`, near-miss rejection)
- `test_heal_backoff_boot_immediate_rehalt_capped` (tiny injected intervals)
- `test_generation_cap_retires_family_and_errors`
- `test_healed_replay_delivers_exactly_once` (needs Phase 1 — the dishonesty pin)
- `test_heal_actor_stops_on_shutdown` (callback configured → `shutdown()` → actor
  terminated; extends the shutdown-test pattern; no task leaks into the next
  `#[serial]` test)
- Implement:
  1. Bus-owned actor task on the channel's paired receiver. Owns the per-family
     monotonic generation counters (family match: exact base OR `base#gen<digits>`
     with near-miss rejection) and the backoff schedule. Per request: (i) mint
     `{base}#gen{N}`; (ii) `unsubscribe` the wedged id; (iii) invoke the callback
     with (base, wedged id, generation, held-below); (iv) WARN once per heal, ERROR
     at the cap with no further heals. Never inline in the halt callback (channel
     send only).
  2. Lifecycle: spawned inside `start_listener` (~1735, alongside the existing
     `tokio::spawn` ~1786) **only when `on_wedge_retired` is configured**;
     `ListenerState` (~1392-1400) gains `Option<JoinHandle<()>>`; `shutdown()`
     (~2456-2487) ends it via `shutdown_tx` (or by dropping the request sender) and
     awaits the handle alongside `handle`/`scan_handle`.
  3. Capability narrowing — **no bus clone** (a clone would keep pool/registry/hwm
     alive independently of the caller's handle): extract `unsubscribe`'s body into
     internal free fn `unsubscribe_core` taking narrowed handles only — the
     projections Vec `Arc`, the `subscriber_modes` registry `Arc` (two *different*
     fields, both needed), the `retired_ids` tombstone `Arc`, `hwm`, the pool, and a
     `ReliableDeliveryConfig` clone (derives Clone; ledger's `bus_name` =
     `config.events_table`; Coordinated branch reads `config.instance_mode`).
     `release_subscriber_lock` (inherent `&self`, ~3499): inline its SQL or extract
     a second pool-only helper the core delegates to — never a full bus handle.
     The inherent `unsubscribe` delegates to `unsubscribe_core`; the actor gets its
     own clones of the sub-handles.

**Out of bounds**: epoch never constructs/subscribes the fresh model (no factory —
option (b)); no epoch_mem changes; no catacloud-side code; no changes to the fetch
exclusions that make the wedge inert (~2116-2150, ~2258-2264).

**Exit**: 9-test battery green; full suite green incl. spec 0030 regression pins
(`Halt` byte-for-byte default, guardrail tests); rustdoc states opt-in contract, gate,
and cap end-state.
**Commits**: `feat(pg): P5 wedge heal — retire-and-notify with fresh generations`
(config+gate) and `feat(pg): heal actor with shutdown-safe lifecycle and narrowed
unsubscribe_core` (actor+lifecycle) — or one commit if the split is unnatural.

## Phase 5 — Trait surface, epoch_mem parity, docs & quality gate (R11, R16)

**Cycle 5.1 — trait default** (compile-level pin first):

- Pin: a minimal third-party `EventBus` impl still compiles (existing test doubles
  serve). Add `unsubscribe` to `pub trait EventBus` (epoch_core/src/event_store.rs
  ~231) as a **defaulted** method returning `Ok(false)`. Rustdoc: default-body
  `Ok(false)` = "backend does not support removal", distinct from an overriding
  backend's `Ok(false)` = "id not found". (`Err(Unsupported)` default impossible:
  `type Error: std::error::Error` has no construction bound — breaking.)

**Cycle 5.2 — epoch_mem parity** (tests first):

- epoch_mem test: subscribe → deliver → unsubscribe → publish → no delivery;
  idempotent second call. Implement on `InMemoryEventBus`: remove from
  `projections: Arc<RwLock<Vec<Box<dyn EventObserver<D>>>>>` under the write lock
  (plain retain/swap_remove by `subscriber_id`); no persisted state to resolve.
  Rustdoc: mem delivery loop holds the projections read lock across the observer
  loop incl. retry sleeps (~618-624), so removal is immediate-when-not-mid-delivery —
  "same contract" (R11) is not "same timing" as pg's next-wake.

**Cycle 5.3 — pg trait override + docs**:

- `impl EventBus for PgEventBus<D>` (~4165-4170): override the default, delegating to
  the inherent `unsubscribe` (generic consumers get real removal, not the no-op).
- `CHANGELOG.md` (Unreleased): `unsubscribe` (pg + mem + trait), heal config/callback,
  `SubscriberNotFound` flip for retired ids, exactly-once fix, CLOUD-259 coordination
  note.
- Final sweep: `cargo fmt`, clippy `-D warnings`, full `cargo test --workspace` with
  DB env, plus a plain `cargo build --workspace` (no-break guardrail).
**Commits**: `feat(event-bus): EventBus::unsubscribe with epoch_mem parity and docs`

## Execution protocol

- Sequence: Phase 0 → 1 → 2 → 3 → 4 → 5, one worktree, no concurrent writers.
- I implement directly; after each phase's validation gate I commit and give a
  one-screen status (tests added, suite state, deviations). The user can interrupt
  between phases; hard blockers (anchor contradictions, scope creep pressure) pause
  for a decision instead of improvising.
- Post-landing coordination (not code): ping CLOUD-259 that heal/retire landed; text
  lives in the CHANGELOG note.
- If the DB rig is down at any point, pause and report — no DB-less faked runs.

## Risk watchlist (from spec §8 — stop conditions, not workarounds)

| Risk | Phase | Stop condition |
|---|---|---|
| Two of three dedup fix shapes are silently catastrophic (stranded backlog / broken gap detection) | 1 | Any test suggests delivered rows went missing → stop, re-derive, do not patch around |
| Silent new `projections` write path | 2 | Site count ≠ six after edits → stop |
| Removal ordering bug leaves stale Arc delivering | 3 | Fence/gate battery red after two fix attempts → stop and re-read the interleaving |
| Heal actor outlives shutdown, leaks into next `#[serial]` test | 4 | Any flaky serial-test contamination → stop, fix lifecycle before proceeding |
| Trait default breaks downstream impls | 5 | `cargo build --workspace` red → stop (would mean the default isn't actually non-breaking) |
