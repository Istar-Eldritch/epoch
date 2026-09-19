# Spec 0031: Observer unsubscribe/retire API and processable ReplayAlways wedges (P5)

**Status:** Implemented (all five phases landed on branch `cloud-262`; `git log 5c27432..HEAD`)
**Created:** 2026-09-16 · **Issue:** Linear CLOUD-262 (parent CLOUD-259) · **Sequel to:** spec 0030 (CLOUD-261)
**Baseline:** worktree `cloud-262`, rev `5c27432`. Anchors below were verified at that rev; the shipped code is the source of truth.
**Siblings:** `specs/0031-cloud262-discovery.md` (open questions, positions in tension), `specs/0031-implementation-plan.md` (TDD step list). Probe evidence in `docs/probes/cloud262-*-20260916.md` (papers 1–2, reports 3–4).

> This spec has been condensed post-implementation. It records what was built and why, not a forward-looking plan. Requirement/decision IDs (R*, OQ*) are retained for cross-reference with the siblings and commit messages.

---

## 1. Problem

Three compounding gaps in the same corner of `PgEventBus`:

1. **No removal path.** The observer registry (`Projections<D>`, `mod.rs`) was push-only; no `unsubscribe`/`retire` existed in any crate. The documented remedy for a wedge was "a fresh `subscribe()`", which leaks the old observer forever. Worse, a wedged `ReplayAlways`+`Halt` subscriber pins `wait_until_all_caught_up` permanently (the gate polls every registered id; a wedged ReplayAlways HWM never advances — report 3 B2).
2. **ReplayAlways wedges are unprocessable (P5).** Spec 0030's P4b self-heal deliberately excluded ReplayAlways wedges: excluded from both the shared batch and the private fetch, so nothing ever drives them again (report 3 B1). `release_halt` is a no-op for them. Catacloud carried 531 lines of integration-layer heal machinery (`integration/src/policy_projection_heal.rs`, CLOUD-259 Phase 3) standing in for this framework gap.
3. **The blocker under both: fresh-subscribe double-delivery.** A fresh `ReplayAlways` subscribe over a burned hole delivered above-hole rows twice (`(3,2),(4,2)` — report 4 B1) and re-wedged. Cause: the subscribe-time catch-up delivers each row once, but the first live wake re-delivers because the listener seeds `SubscriberState` from the pinned HWM with an empty `processed_ahead`, blind to what catch-up applied. Every P5 heal triggers this by construction (a heal's trigger is an unfilled hole), so shipping P5 without the fix would automate silent fold corruption.

The double-delivery bug was never ticketed on Linear; it reproduced byte-for-byte at `5c27432`. The fix (Part A) is a hard prerequisite for P5 (Part C), and the retire API (Part B) is what the heal is built from.

---

## 2. Requirements (as shipped)

Part A — dedup (R1–R3); Part B — retire API (R4–R10); Part C — P5 heal (R11–R15); cross-cutting gate (R16).

- **R1.** A fresh `ReplayAlways` `subscribe()` over an unfilled hole delivers every above-hole row **exactly once** across catch-up plus all live wakes, while position stays pinned below the hole (CLOUD-227 unbroken-prefix guard).
- **R2.** Exactly-once holds on every process boot / listener restart: both catch-up call sites (`subscribe()` and the R2 startup replay pass) hand off the delivered-above-prefix set to the live seed. Catch-up passes themselves remain **at-least-once per pass** (one copy per pass that runs); the handoff only guarantees the live wake loop adds no further copies. Cross-restart exactly-once is unachievable without persisting the delivered set (out of scope; no migration). *(Wording amended 2026-09-17, Phase 1 review cycle 1.)*
- **R3.** Non-regressive: hole-free streams unchanged; the shared floor and pinned position never advance past a hole because of the handoff; `Checkpointed` semantics and the `GapPolicy::Halt` default unchanged.
- **R4.** `PgEventBus::unsubscribe(id) -> Result<bool, PgEventBusError>`, valid on Async and Inline buses, idempotent (`Ok(true)` removed / `Ok(false)` unknown). (OQ-2)
- **R5.** Identify observers by id **without locking any observer's mutex**: a lock-free id→observer registry populated by `subscribe()`, with `fast_forward_all_subscribers` retrofitted onto it (removing its pre-existing lock-behind-`on_event` scan).
- **R6.** Remove the id's in-memory participation — `subscriber_modes`, `hwm`, and **all** same-id observer `Arc`s in the projections Vec (`Arc::ptr_eq`, under the outer mutex) — so `wait_until_all_caught_up` resolves again and per-id readiness returns `SubscriberNotFound`. The registry retains every same-id `Arc` (not just the last) so `ptr_eq` removal catches the delivering observer, not just an inert duplicate.
- **R7.** No listener-task-lifetime residue: prune the retired id from `subscriber_states`, `pending_checkpoints`, `checkpoint_cache`, `last_event_ids` via a cross-task retired-id tombstone consulted at each wake's init pass, so it stops pinning the shared floor and P4b's private fetch.
- **R8.** Persisted-state policy: unresolved `epoch_event_bus_gap_timeouts` rows resolved with `resolved_by='unsubscribe'` (best-effort, logged); checkpoint and DLQ rows **retained** (audit; resurrection hazard via `flush_all_pending_checkpoints`). Same-id re-subscribe starts a clean lifecycle: `Checkpointed` resumes from the retained row, `ReplayAlways` replays from 0, no first-registration-wins warning. (OQ-8: retain)
- **R9.** Timing is **documented, not fixed**: removal takes effect next wake; the current wake's pre-unsubscribe snapshot may still deliver, **except** the one wake that consumes the prune marker, which fences that id's snapshotted `Arc` from its own dispatch. An `unsubscribe` racing an in-flight same-id `subscribe()` **wins** — the push sites re-check and abort the subscribe with `Err(SubscriberNotFound)` (existing variant, non-breaking). A same-id re-subscribe racing a wake may be inert for up to one wake. Retiring the last subscriber makes `wait_until_all_caught_up` trivially `Ok(true)` (documented hazard). (OQ-10)
- **R10.** Coordinated buses: best-effort `release_subscriber_lock`, documented unreliable (session-scoped advisory locks on pooled connections — a pre-existing defect). (OQ-11)
- **R11.** epoch_mem parity: `InMemoryEventBus::unsubscribe` with the same contract, reachable via `epoch_core::EventBus` (defaulted method, default body `Ok(false)`; mem overrides, pg's `impl EventBus` delegates to the inherent method). Mem timing is immediate-when-not-mid-delivery, not "next wake". (OQ-6)
- **R12.** Heal trigger gate: fires **only** for `ReplayAlways` + `FailClosed` + `GapPolicy::Halt` wedged by `HaltReason::GapUnproven`, once per (generation, gap) via the set-once `halt_fired` site. Never for `SkipAfterBackstop`, deserialize/observer wedges, `FenceCleared`, or `Released`.
- **R13.** Heal shape — **option (b)**: on the gated trigger epoch retires the wedged observer and invokes an application `WedgeRetiredCallback` with context (base id, wedged id, generation, held-below sequence). Epoch never constructs/clears/`subscribe()`s the fresh model. Fresh ids follow `{base}#gen{N}` with near-miss rejection (`base` must not match `base-suffix`). (OQ-9)
- **R14.** Configurable policy: backoff (default: immediate on a boot-generation wedge, 30 s then 60 s cap on heal-generation re-halts) and a `max_generations` cap whose end-state is retire-the-chain + ERROR + no further heals. Heal runs in a bus-owned actor task fed by a channel from the fire site — **never inline** in the awaited halt callback. Heal is active iff `on_wedge_retired` is `Some`; `wedge_heal: None` uses the default schedule (no separate enable flag). (OQ-4, OQ-5)
- **R15.** Observability: one WARN per heal, ERROR at the cap. Any new `HaltReason` variant must go on the `#[non_exhaustive]` enum source-compatibly. *(As shipped, no new variant was added — the optional `WedgeSuperseded` was left out.)*
- **R16.** Quality gate: rustdoc on all new public API; `cargo clippy -- -D warnings` clean; no `unwrap()`/`expect()` outside tests; **no new dependencies**; CHANGELOG documents the new surface and the `SubscriberNotFound` flip.

---

## 3. Key decisions (settled 2026-09-16 with Ruben; full record in the discovery doc)

- **OQ-1 in scope:** the dedup fix ships as Phase 1, not an external ticket.
- **OQ-2 idempotent:** `unsubscribe -> Result<bool, _>` (`Ok(true)`/`Ok(false)`); readiness keeps `SubscriberNotFound` (query vs teardown asymmetry, intentional).
- **OQ-3 deferred:** no retire-by-prefix; exact-id composes with `#genN`. A family-match wrapper is a thin future addition.
- **OQ-4:** catacloud-mirroring backoff defaults + a `max_generations` cap (catacloud has none) ending in retire + ERROR.
- **OQ-5:** opt-in by construction — registering the callback *is* the opt-in; `GapPolicy::Halt` byte-for-byte unchanged without it.
- **OQ-6:** defaulted trait method, body `Ok(false)` (`Self::Error` has no construction bound, so `Err(Unsupported)` would be breaking).
- **OQ-7 out of epoch's scope:** catacloud's adopt-and-delete timing is catacloud's call. Epoch-side coordination item stands: **ping CLOUD-259 when 0031 lands**; hand over paper 2 §8.2's two interim guards (stale-generation CAS; retired-check via `SubscriberNotFound`) for any dual-healer window.
- **OQ-8 retain:** checkpoint + DLQ rows retained; gap-ledger rows resolved `'unsubscribe'`.
- **OQ-9 option (b):** epoch retires, the application re-subscribes. Option (a) factory heal deferred (not rejected).
- **OQ-10 document now:** empty-registry `Ok(true)` hazard documented (R9); guard is a possible follow-up.
- **OQ-11 defer:** no advisory-lock ownership fix; R10 documents best-effort release.

---

## 4. Scope boundaries (unchanged from plan)

**Out of scope:** catacloud-side code (coordination item only); option (a) factory heal (deferred); in-framework retire-by-prefix (deferred); advisory-lock ownership fix (deferred); `release_halt` / `SkipAfterBackstop` semantics (spec 0030 surface, untouched); epoch_mem gap machinery (none exists — P5 is `epoch_pg`-only); any new migration (ledger `resolved_by` already exists at `m009:43`).

**Load-bearing constraints honoured:** never stop `catch_up_from_checkpoint` delivering above a hole; never move the live fetch floor/cursor past a hole (both strand fail-open/Checkpointed backlogs and break CLOUD-227/gap detection); never modify the resolver's `SkipAfterBackstop` arm or `release_halt`; never run heal work inline in `fire_on_halt`.

---

## 5. What was built (5 phases, 3 parts)

Phase 1 (Part A) ∥ Phase 2 (Part B prerequisite); Phase 3 needs 2; Phase 4 needs 1 and 3; Phase 5 closes out. Delivered TDD, DB-gated (`EPOCH_REQUIRE_DB=1`, `#[serial]`, `isolated_events_table`) in `epoch_pg/tests/pgeventbus_integration_tests.rs`.

**Phase 1 — exactly-once fresh subscribe (R1–R3).** `catch_up_from_checkpoint` returns the exact set of sequences delivered above the pinned prefix; both catch-up call sites hand it off through a `pending_delivered_sets` carrier (beside `hwm`/`subscriber_modes`) into `SubscriberState::new_with_event_id`, consulted at the live-wake dedup point. **Rejected:** an O(1) range watermark — late-materialized sequences (`scan_late_materialized_gaps`) would be skipped forever (silent loss); the state must record the exact delivered sequences, never an upper bound. The hole-blind CLOUD-225 comment was corrected.

**Phase 2 — lock-free id→observer registry (R5).** `subscriber_modes` extended to `HashMap<String, (SubscriptionMode, Vec<Arc<Mutex<dyn EventObserver<D>>>>)>` (every same-id Arc, since delivery is first-registration-wins). `fast_forward_all_subscribers` retrofitted to resolve from it. A capture-based `Arc::ptr_eq` removal helper (`remove_captured_observers`) added: caller clones the id's Arcs out under the outer mutex, then the helper removes them from the projections Vec — it does not look the id up or drop the registry entry.

**Phase 3 — `unsubscribe` retire API (R4, R6–R10).** In `unsubscribe.rs`. Removal order: capture Arcs → remove from projections Vec → drop `subscriber_modes` entry (readiness flips) → drop `hwm` and `pending_delivered_sets` → insert tombstone (`retired_ids`) → best-effort gap-ledger resolve (demoted to logged best-effort so a transient DB error can't leave a subscriber removed-everywhere-but-retired; ordering hardened in f92aaf1) → Coordinated best-effort lock release → checkpoint/DLQ retained. Inline buses (no listener) stop before the tombstone. The listener's wake init pass consumes the tombstone: prune the four listener-lifetime maps, **conditionally** re-clear `hwm` only when the id is not currently registered (the ReplayAlways advance path can resurrect it; a same-id re-subscribe owns a fresh hwm lifecycle that must not be cleared — amended 2026-09-17 Phase 3), fence the id's snapshotted Arc from this wake's dispatch, then drop the marker. The async subscribe push sites re-check registry membership post-push and abort with `Err(SubscriberNotFound)` if a concurrent unsubscribe won.

**Phase 4 — P5 wedge heal (R12–R15).** Config (`config.rs`): `WedgeRetiredCallback` trait, `WedgeRetiredInfo`, `WedgeHealPolicy` (`max_generations` + backoff), `on_wedge_retired`/`wedge_heal` fields (both default `None`). The `fire_on_halt(GapUnproven)` site only gate-checks (reusing the wake's existing mode read — no new observer lock) and emits a `HealRequest` over an `Option<mpsc::UnboundedSender>` on `BatchContext`. A single bus-owned heal actor (`heal.rs`, `run_heal_actor`) owns per-family generation counters and backoff: mints `{base}#gen{N}`, calls unsubscribe, invokes the callback, WARNs per heal, ERRORs at the cap. Deferred (backed-off) requests are deduped by subscriber_id and re-validated against live registry handles at fire time — dropped as stale if retired/re-subscribed during the window (f92aaf1). Lifecycle: actor spawned in `start_listener` only when configured, joined on `shutdown()`; capability-narrowed via `unsubscribe_core` (narrowed handles, never a full `PgEventBus` clone, to avoid keeping shared state alive).

**Phase 5 — trait surface, mem parity, docs gate (R11, R16).** `epoch_core::EventBus::unsubscribe` defaulted (`Ok(false)`); `InMemoryEventBus` overrides (write-lock retain); pg's `impl EventBus` delegates to the inherent method. CHANGELOG updated; fmt/clippy/test clean.

---

## 6. Known gap (stated plainly, not softened)

**The inline-mode (`DispatchMode::Inline`) subscribe/retire race has no deterministic test.** The async-mode equivalent (push-then-recheck at the async push site) is covered. Inline's push-then-recheck window has no guaranteed-yield await point, so there is no reliable way to interleave a concurrent unsubscribe in a test without adding a production test-only delay hook. That hook was **not** added — it was left as an explicit open scope decision, documented in commit f92aaf1's message. The invariant (an observer Arc in the projections Vec always has a live registry entry) is enforced identically on both paths; only the *test trigger* is missing for inline.

Other tracked cosmetic follow-up (now fixed): `rehalt_count` was not rolled back when a deferred request was dropped as stale, which could walk the backoff schedule slightly fast (invisible with a single-entry schedule) — fixed in bcc7c7d, which decrements the count on a stale-drop.

---

## 7. Codebase map (verified anchors)

New/primary files: `epoch_pg/src/event_bus/unsubscribe.rs` (retire + `unsubscribe_core` + `remove_captured_observers` + `release_subscriber_lock_core`), `epoch_pg/src/event_bus/heal.rs` (`HealRequest`, `run_heal_actor`), config surface in `epoch_pg/src/event_bus/config.rs`. Registry/dedup/tombstone/fire-site/lifecycle wiring in `epoch_pg/src/event_bus/mod.rs`. Seed constructor + delivered-set param in `subscriber_state.rs`. Trait in `epoch_core/src/event_store.rs`; parity in `epoch_mem/src/event_store.rs`. Tests: `epoch_pg/tests/pgeventbus_integration_tests.rs` (+ `epoch_core/tests/event_bus_unsubscribe_default_tests.rs`). No migration touched; ledger resolve reuses `epoch_event_bus_gap_timeouts.resolved_by` (`m009:43`).
