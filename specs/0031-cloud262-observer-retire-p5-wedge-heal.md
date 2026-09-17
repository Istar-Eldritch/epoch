# Spec 0031: Observer unsubscribe/retire API and processable ReplayAlways wedges (P5)

**Status:** Draft
**Created:** 2026-09-16
**Discovery:** `specs/0031-cloud262-discovery.md`
**Issue:** Linear CLOUD-262 (parent CLOUD-259) · **Sequel to:** spec 0030 (CLOUD-261)
**Baseline:** worktree `cloud-262`, rev `5c27432` (CLOUD-261 merged to main). Every
`path:line` anchor below was read and verified at this rev.

**Verified evidence (prober reports, 2026-09-16, cited throughout):**
`docs/probes/cloud262-retire-semantics-paper-20260916.md` (retire semantics, "paper 1"),
`docs/probes/cloud262-p5-design-space-paper-20260916.md` (P5 design space, "paper 2"),
`docs/probes/cloud262-wedge-retire-readiness-probe-20260916.md` (live-PG probe, "report 3"),
`docs/probes/cloud262-fresh-subscribe-dedup-probe-20260916.md` (live-PG probe, "report 4").

---

## 1. Problem Statement

Two capability gaps compound in the same corner of `PgEventBus`, and their interaction
is worse than either alone.

**No removal path.** There is no way to unregister a subscriber. The registry
(`type Projections<D>`, `epoch_pg/src/event_bus/mod.rs:1304`) is push-only; zero
`unsubscribe`/`retire` symbols exist across all five crates. The vendored-documented
remedy for a wedged subscriber is "a fresh `subscribe()`" (`mod.rs:2129-2132`), which
leaves the old observer registered forever. Re-subscribing under a new id — what
catacloud's CLOUD-259 Phase 3 heal does with `#genN` ids — therefore accumulates one
inert registered observer per heal, each costing ≈8.6–12 µs per subscriber per delivery
wake in registry-scan work (report 3, B4; upper bound, within noise at N ≤ 10, +0.4–0.6
ms/wake at N = 50). More seriously, a wedged `ReplayAlways`+`Halt` subscriber **pins
`wait_until_all_caught_up` forever**: the gate snapshots `subscriber_modes`
(`mod.rs:2966-2972`) and polls every registered id's position, and a wedged
ReplayAlways HWM never advances — report 3 B2 measured `Ok(false)` in 3/3 rounds with
the wedged id registered and resolvable resolution on an identical registry without it.

**ReplayAlways wedges are unprocessable (P5).** P4b's self-heal deliberately excludes
ReplayAlways wedges (`mod.rs:2129-2132`, "the `ReplayAlways` floor-exclusion analogue
lands in P5"). A wedged ReplayAlways subscriber is excluded from the shared batch
(`wedged_now`, `mod.rs:2260-2264`) *and* from the private fetch, so nothing drives it
ever again: report 3 B1 reproduced the wedge (GapUnproven at 5.015 s with
`gap_timeout: 5 s`, position pinned at 1 while head reached 6, zero post-wedge
deliveries, exactly one halt). `release_halt` is a documented no-op for it
(`mod.rs:2646-2653`), and epoch's own non-test code never calls it (only integration
tests do). Catacloud carries 531 lines of
integration-layer heal machinery (`integration/src/policy_projection_heal.rs`, CLOUD-259
Phase 3) standing in for exactly this framework gap.

**The blocker underneath both: fresh-subscribe double-delivery.** Spec 0030 §4 deferred
a known bug as "ticketed separately" — no ticket was ever filed, and report 4 B1
reproduced it byte-for-byte at `5c27432`: a fresh `ReplayAlways` subscribe over a burned
hole delivers `[(1,1),(3,2),(4,2),(5,1),(6,1)]` and then re-wedges at the same hole.
Report 4 B2a/B2b localized it: the subscribe-time catch-up delivers each above-hole row
exactly once; the **first live wake** re-delivers them because the listener seeds
`SubscriberState` from the pinned HWM with a born-empty `processed_ahead`
(`mod.rs:2063-2111`, `subscriber_state.rs:170-186`), blind to what catch-up applied.
The blast radius is every process boot of a ReplayAlways subscriber over an open
historical burn — and **every P5 heal triggers it by construction**, because a heal's
trigger is an unfilled hole. Shipping P5 without this fix automates deterministic,
silent fold corruption: a visible inert wedge that "heals" into double-applied state.

**Findings beyond the ticket (binding, from the probe/paper verdicts):**

1. The double-delivery bug is **not ticketed on Linear** (searched 2026-09-16); it
   reproduces exactly at `5c27432` and its mechanism is the missing
   subscribe()→listener handoff of the delivered-above-prefix range (report 4 B1/B2).
2. The listener state maps `checkpoint_cache`, `pending_checkpoints`,
   `subscriber_states`, `last_event_ids` are **listener-task-lifetime**, declared once
   before the select loop (`mod.rs:1872-1885`) — not per-wake. A retire API must prune
   them across tasks or a lingering state keeps feeding the shared floor
   (`mod.rs:2206-2211`) and P4b's private fetch (`mod.rs:2133-2150`).
3. Retire must identify observers **without locking them**: `invoke_observer_once`
   (`retry.rs:43-60`) holds the observer mutex across the whole `on_event` await, so
   the only identification path today (lock each observer, as `fast_forward_all_subscribers`
   does at `mod.rs:3025-3033`) blocks behind any in-flight `on_event` (a gap wedge
   itself holds no mutex — it is a state bit, `subscriber_state.rs:199-202`, report 3
   B3 — but the scan still can't tell that apart from a slow handler without locking).
4. Shutdown/reconnect `flush_all_pending_checkpoints` can resurrect a deleted
   checkpoint row (paper 1 V3) — motivating **retain** (not delete) checkpoint-row
   policy at retire.
5. Ledger rows fire `RebuildNeededCallback` once, then auto-resolve with
   `'gap_detection'` (`mod.rs:447-468`); a retired id's unresolved rows would fire one
   zombie callback each — retire should resolve them with `resolved_by='unsubscribe'`.
6. Advisory-lock release is **already broken** (session-scoped `pg_try_advisory_lock`
   on pooled connections; `release_subscriber_lock` at `mod.rs:3499-3512` has zero
   callers and returns `false` cross-session) — unsubscribe's lock story must be
   defined against that reality, not as a new defect.
7. Recommended P5 shape is **option (b): epoch-managed auto-retire + heal callback**
   (paper 2 §2.3): single-actor race-free by construction, bounds memory by removing the
   wedged observer, and makes CLOUD-262's two asks one mechanism. Epoch cannot construct
   a consumer's `dyn EventObserver` (registry is `Arc<Mutex<dyn EventObserver<D>>>`;
   epoch's own contract: "Epoch provides the trigger, not the rebuild",
   `config.rs:247`), so the application must own the re-subscribe.
8. epoch_mem parity is trivial (`Box<dyn EventObserver>` in an `RwLock<Vec>`, push-only
   subscribe, `epoch_mem/src/event_store.rs:557`, `755-768`) and keeps the
   generation-chain consumer story portable to in-memory test rigs (paper 1 §7).

**Positions in tension** (recorded in the discovery doc; recommendations made in §5,
final calls settled 2026-09-16 and recorded in §7): (i) dedup fix in-scope vs
prerequisite ticket; (ii) checkpoint rows on retire — retain vs delete.

---

## 2. Requirements

Each requirement is a single, independently verifiable claim. R1–R3 are the Part A
dedup fix; R4–R10 the retire API; R11–R15 the P5 heal; R16 the cross-cutting quality
gate.

- **R1.** A fresh `subscribe()` of a `ReplayAlways` subscriber over a stream containing
  an unfilled hole must deliver every row above the hole **exactly once** across the
  subscribe-time catch-up pass and all subsequent live wakes (observed counts
  `(3,1),(4,1)`, not `(3,2),(4,2)`), while the subscriber's position remains pinned
  below the hole per the CLOUD-227 unbroken-prefix guard.
- **R2.** The live-wake exactly-once guarantee of R1 must hold on **every process
  boot** and listener restart of a `ReplayAlways` subscriber over an open historical
  burn: both catch-up call sites (`subscribe()` and the R2 startup replay pass,
  `mod.rs:1846-1856`) must hand off what they delivered above the prefix to the live
  state seed, so the live wake loop never duplicates a catch-up-applied row. Catch-up
  passes themselves remain **at-least-once per pass** while the prefix stays pinned
  (a row above the hole is delivered once per catch-up pass that runs — subscribe-time
  plus one per listener boot): the handoff is in-memory and `catch_up_from_checkpoint`'s
  delivery behaviour is fenced unchanged (no migration in scope), so per-row copies are
  bounded by the number of catch-up passes, never by the live loop. (Scope note amended
  2026-09-17, Phase 1 review cycle 1 — the prior wording implied cross-restart
  exactly-once, which is unachievable without persisting the delivered set.)
- **R3.** The dedup handoff must be non-regressive: hole-free streams deliver
  byte-for-byte unchanged; the shared fetch floor and the pinned position must never
  advance past a hole because of it; `Checkpointed` catch-up semantics and the
  `GapPolicy::Halt` default are unchanged (regression pins).
- **R4.** `PgEventBus` must expose `unsubscribe(subscriber_id)` that removes a
  registered observer from the bus, valid on both `DispatchMode::Async` and
  `DispatchMode::Inline` buses, idempotent for unknown ids — contract
  `Result<bool, PgEventBusError>` with `Ok(true)` removed / `Ok(false)` unknown
  (settled: OQ-2, 2026-09-16).
- **R5.** The bus must identify registered observers by subscriber id **without
  acquiring any observer's inner mutex**: `subscribe()` must populate a lock-free
  id→observer registry, and `fast_forward_all_subscribers` must be retrofitted to
  resolve observers from it (eliminating its pre-existing scan that blocks behind
  in-flight `on_event` handlers).
- **R6.** Unsubscribe must remove the retired id's **in-memory registry participation**
  — the `subscriber_modes` entry, the `hwm` entry (`ReplayAlways`), and **all**
  same-id observer `Arc`s in the projections Vec (identified by `Arc::ptr_eq` under the
  outer mutex) — such that `wait_until_all_caught_up` resolves again (no longer pinned
  by the retired id) and per-id readiness methods (`subscriber_lag`,
  `wait_until_caught_up`) return `SubscriberNotFound` for it. The Phase 2 registry
  retains **every** same-id observer `Arc` (not just the last one registered) so that
  `ptr_eq` identification during unsubscribe is complete — unsubscribe must remove the
  delivering observer, not just an inert duplicate.
- **R7.** Unsubscribe must prevent **listener-task-lifetime residue**: the retired id
  must be pruned from `subscriber_states`, `pending_checkpoints`, `checkpoint_cache`,
  and `last_event_ids` via a cross-task mechanism (recommended: a shared retired-id
  set consulted at each wake's init pass, `mod.rs:2063` region), so a retired idle
  subscriber stops pinning the shared fetch floor and a retired wedged
  `Checkpointed` subscriber stops entering P4b's private fetch.
- **R8.** Unsubscribe's **persisted-state policy**: unresolved
  `epoch_event_bus_gap_timeouts` rows for `(bus_name, subscriber_id)` must be resolved
  with `resolved_by='unsubscribe'` (silencing zombie rebuild callbacks); checkpoint
  rows and DLQ rows must be **retained** (audit; resurrection hazard, paper 1 V3/V4);
  same-id re-subscribe after retire must start a clean new lifecycle with no
  first-registration-wins warning — a `Checkpointed` re-subscribe resumes from the
  retained row, a `ReplayAlways` re-subscribe replays from 0 (retain-vs-delete
  settled: retain, OQ-8, 2026-09-16). Note: gap-timeout rows are only ever written
  when a `TimeoutBackstop` skip is taken (`mod.rs:330-334,869-872`); a `FailClosed`+
  `Halt` subscriber refuses the backstop and writes none (`subscriber_state.rs:428-437`),
  so the resolve-marker test needs a fail-open or `SkipAfterBackstop` subject, not the
  wedged `ReplayAlways`+`Halt` subscriber used by the other Phase 3 tests.
- **R9.** The timing semantics must be **documented rustdoc, not fixed**: removal takes
  effect at the next wake; the current wake may still deliver to and advance the
  retired observer via its pre-unsubscribe snapshot — **except** the one wake that
  consumes the id's pending-prune marker, which must fence that snapshotted `Arc`
  from its own dispatch (the snapshot necessarily predates the unsubscribe, so it is
  always the retired Arc; a re-registered replacement's Arc was pushed after the
  snapshot and joins delivery from the next wake); an unsubscribe racing an
  in-flight `subscribe()` of the same id wins — the push sites' post-push re-check
  aborts the subscribe with `Err(SubscriberNotFound)` (the existing variant, no new
  enum member, non-breaking; rustdoc: re-issue the subscribe if still wanted); a
  same-id re-subscribe racing an in-flight wake may be inert for up to one wake;
  retiring the last subscriber makes `wait_until_all_caught_up` return `Ok(true)`
  trivially (empty-registry hazard).
- **R10.** On `InstanceMode::Coordinated` buses, unsubscribe must attempt
  `release_subscriber_lock` **best-effort** and document that the release is
  unreliable (session-scoped advisory locks on pooled connections); Coordinated
  subscribe behaviour must be unchanged.
- **R11.** epoch_mem parity: `InMemoryEventBus` must support `unsubscribe` with the
  same contract, and the operation must be reachable through the
  `epoch_core::EventBus` trait (settled: a defaulted trait method whose default body
  returns `Ok(false)` — `Self::Error` has no construction bound, so an
  `Err(Unsupported)` default would require a breaking bound — with
  `InMemoryEventBus` overriding and `PgEventBus`'s `impl EventBus` also overriding by
  delegation to its own inherent `unsubscribe`; non-breaking; OQ-6, 2026-09-16). Note:
  the mem-bus delivery loop holds its projections read lock across the observer loop
  including retry sleeps (`epoch_mem/src/event_store.rs:618-624`), so mem unsubscribe
  is immediate-when-not-mid-delivery rather than "next wake" — "same contract" is not
  "same timing".
- **R12.** P5 heal trigger gate: the heal must fire **only** for a `ReplayAlways` +
  `FailClosed` + `GapPolicy::Halt` subscriber wedged by `HaltReason::GapUnproven`,
  exactly once per (generation, gap) via the set-once `halt_fired` fire site
  (`mod.rs:766-774`); it must never fire for `SkipAfterBackstop` subscribers (they
  never gap-wedge, `subscriber_state.rs:413-427`), never for deserialize/observer
  wedges, and never for `FenceCleared` or `Released`.
- **R13.** P5 heal shape (option b): on the gated trigger, epoch must **retire the
  wedged observer** (via R4) and invoke an application-provided heal callback with
  enough context to re-subscribe (family base id, wedged id, generation, held-below
  sequence); epoch must **not** construct, clear, or `subscribe()` the fresh model
  itself; fresh ids must follow the `{base}#gen{N}` family convention with near-miss
  rejection (a heal for `base` must never match `base-suffix`), matching catacloud's
  CLOUD-259 Phase 3 scheme.
- **R14.** P5 heal policy must be configurable: a backoff schedule (default mirroring
  catacloud Phase 3: immediate on a boot-generation wedge, 30 s then 60 s cap on
  heal-generation re-halts — settled as the shipped defaults, OQ-4, 2026-09-16) and a
  `max_generations` cap whose end-state is retire-the-chain + ERROR + no further
  heals; the heal work must run in a spawned task, never inline in the halt callback
  (the callback is awaited on the bus's batch loop) — the fire site emits a heal
  request over a channel to a single bus-owned heal-actor task, which owns the
  counters, backoff, and callback invocation (see Phase 4). The heal is active iff
  `on_wedge_retired` is `Some`; a `Some` callback with `wedge_heal: None` uses the
  default backoff schedule above — there is no separate enable flag.
- **R15.** Observability: one WARN per heal (old id, new id, generation,
  held-below sequence) and an ERROR at the generation cap; any new `HaltReason`
  variant (e.g. `WedgeSuperseded`) must be added to the `#[non_exhaustive]` enum
  (`config.rs:59-60`) without breaking existing wildcard matches.
- **R16.** Cross-cutting quality gate: all new public API carries rustdoc (contracts
  from R8/R9 included); `cargo clippy -- -D warnings` clean; no `unwrap()`/`expect()`
  outside tests; **no new dependencies**; `CHANGELOG.md` documents the new public
  surface (`unsubscribe`, trait method, heal config/callback) and the observable
  `SubscriberNotFound` flip for retired ids.

---

## 3. Success Criteria

- [ ] A fresh `ReplayAlways` `subscribe()` over a burned hole delivers every above-hole
      row exactly once (counts `(3,1),(4,1)`) while its position stays pinned below the
      hole — DB-gated integration test green (today: `(3,2),(4,2)` per report 4 B1).
- [ ] A process restart (listener R2 pass) over an open burn leaves the live wake
      loop adding no further copies: above-hole rows applied by a catch-up pass are
      re-delivered only by later catch-up passes (subscribe-time and one per listener
      boot — at-least-once per pass; the handoff is in-memory and no migration is in
      scope), never by the live wake path (test green). (Amended 2026-09-17, Phase 1
      review cycle 1: the original wording — "does not re-deliver catch-up-applied
      rows" — is unachievable within Phase 1's own fences, which forbid changing what
      `catch_up_from_checkpoint` delivers and forbid a migration.)
- [ ] `unsubscribe` on a wedged `ReplayAlways`+`Halt` subscriber un-pins
      `wait_until_all_caught_up` on the live bus without restarting it (report 3 B2's
      inference turned into a passing test).
- [ ] Post-retire: `subscriber_lag`/`wait_until_caught_up` on the old id return
      `SubscriberNotFound`; the id's unresolved gap-ledger rows carry
      `resolved_by='unsubscribe'`; the checkpoint row (if any) and DLQ rows still exist.
- [ ] A retired *idle* `Checkpointed` subscriber no longer pins the shared fetch floor
      (post-retire publishes are fetched from the healthy subscribers' floor, no
      permanent one-batch-per-wake waste).
- [ ] With a heal callback configured, a `GapUnproven` wedge on `ReplayAlways`+`Halt`
      retires the wedged observer and invokes the callback exactly once with a fresh
      `{base}#genN` id; a heal over the hole delivers exactly once (R1 holds across the
      heal); `SkipAfterBackstop` and deserialize/observer wedges never trigger it.
- [ ] Reaching `max_generations` retires the family, logs ERROR, and stops healing.
- [ ] `InMemoryEventBus::unsubscribe` removes the observer (no further delivery) and a
      default-implemented `EventBus::unsubscribe` compiles without downstream breakage.
- [ ] Full suite green with `EPOCH_REQUIRE_DB=1`; `cargo clippy -- -D warnings` clean;
      `cargo fmt` clean; CHANGELOG updated.
- [ ] `fast_forward_all_subscribers` completes while a wedged observer's `on_event` is
      parked (in-flight, mutex held) — R5's lock-free identification, exercised by a
      Phase 2 test.
- [ ] Unsubscribe's timing semantics (next-wake effect, current-wake tail delivery,
      up-to-one-wake re-subscribe race) are documented in rustdoc on `unsubscribe`
      (R9).
- [ ] Best-effort advisory-lock release on `Coordinated` unsubscribe, and its
      cross-session unreliability, are documented in rustdoc (R10).

---

## 4. Scope & Boundaries

**In scope:**

- Part A — the exactly-once fresh-subscribe-over-open-holes fix (catch-up→live dedup
  handoff), covering both catch-up call sites and the hole-blind CLOUD-225 comment.
- The retire API: exact-id `unsubscribe` on `PgEventBus` with the full removal
  inventory (registry, Vec, `hwm`, listener tombstones, ledger resolve, advisory-lock
  best-effort), the id→Arc registry prerequisite, and the
  `fast_forward_all_subscribers` retrofit.
- P5 (option b): wedge-heal config surface (callback + policy), `GapUnproven`-only
  trigger, `{base}#gen{N}` id convention, backoff + cap, observability.
- epoch_mem parity and the `EventBus` trait surface.
- Docs: rustdoc contracts, CHANGELOG, and the catacloud coordination note (CLOUD-259
  ping text lives in the CHANGELOG/phase output, not in catacloud).

**Out of scope** (per the discovery doc and §7's settled decisions):

- **Catacloud-side code changes** — none. Deleting
  `integration/src/policy_projection_heal.rs` (531 lines) is catacloud's move after
  soak; tracked as a coordination item: **ping CLOUD-259 when spec 0031 lands**
  (the ticket's coupling warning) and hand over the two interim guards from paper 2
  §8.2 (stale-generation CAS; retired-check via `SubscriberNotFound`) for any window
  where both healers coexist.
- **Option (a) factory heal** — epoch-internal auto-resubscribe behind an observer
  factory is recorded as deferred, not rejected (paper 2 §2.3): same application
  surface, plus a double-healer hazard, plus inert-observer accumulation unless it
  also gains retire.
- **Retire-by-prefix in-framework** — deferred to the consumer layer (settled OQ-3;
  exact-id composes with the `#genN` scheme). If wanted later, it is a thin
  family-match wrapper over the exact-id primitive added in Phase 3.
- **Advisory-lock ownership fix** (dedicated lock connection per subscription) —
  deferred (settled OQ-11); unsubscribe documents best-effort release.
- **`release_halt` behaviour changes** — spec 0030 surface, untouched (still writes
  rows for unregistered/retired ids, documented).
- **`SkipAfterBackstop` semantics** — spec 0030 surface, untouched; the heal is
  structurally unreachable for that class.
- **epoch_mem gap machinery** — none exists (no sequences, no wedges); P5 is
  `epoch_pg`-only.
- **Any new migration** — m001–m014 are untouched; the ledger resolve reuses the
  existing `epoch_event_bus_gap_timeouts` columns (`m009:35-48`, `resolved_by` at
  `m009:43`).

---

## 5. Solution Approach (advisory)

**One spec, three parts, phased like spec 0030's A+B.** Part A is the dedup handoff
fix; Part B is the retire API; Part C is P5 as option (b). The ordering is not
cosmetic: Part A is a hard prerequisite for Part C (every heal runs a fresh subscribe
over the heal's own hole — un-fixed, P5 automates silent fold corruption, report 4
B1/paper 2 §4.3), and Part B is a hard prerequisite for Part C (the heal *is* a
retire-and-notify). Part A and the Part B prerequisite (the id→Arc registry) are
mutually independent and can proceed in parallel.

**Part A — close the subscribe()→listener handoff.** The catch-up pass delivers each
row exactly once (report 4 B2a); the bug is that the one-time live state seed
(`mod.rs:2063-2111`) starts from the pinned HWM with an empty `processed_ahead`
(`subscriber_state.rs:170-186`). The fix shape is: carry the **exact set of sequences
delivered above the pinned prefix** (a set or coalesced-interval structure —
implementer's choice) from the catch-up call sites into the seed, consulted beside the
`processed_ahead.contains` test at `mod.rs:533` — never an upper bound alone. An O(1)
**range watermark** (single max-sequence-delivered value) was considered and
**rejected**: catch-up only sees currently-visible rows, and sequences can
materialize late (`scan_late_materialized_gaps`, `mod.rs:394`; gap machinery
`subscriber_state.rs:410-437`). Repro: burn seq 2, hold seq 4 open in an uncommitted
txn, publish 3/5/6, subscribe (catch-up delivers 3, 5, 6; a range watermark would be
set to 6), then commit seq 4 — the first live wake finds `4 ≤ 6` and skips it
forever, a silent loss. Two shapes are explicitly forbidden because they break
load-bearing behaviour: stopping catch-up delivery above a hole (it would strand the
backlog of every fail-open/Checkpointed subscriber behind any permanent hole) and
moving the live fetch cursor/floor past the hole (it would violate CLOUD-227 ordering
and gap detection). Seeding `processed_ahead`-style state is the sanctioned
representation ("applied but not contiguous", spec 0027 R1). The hole-blind CLOUD-225
comment at `mod.rs:2032-2037` is amended in the same change.

**Part B — retire as an inventory, not a `Vec::retain`.** The removal itself is
trivial; the design constraints are (i) identification without observer mutexes —
solved by extending the per-bus registry that already exists precisely because
observer mutexes are held across `on_event` (`subscriber_modes`, `mod.rs:1429`, doc
`1416-1428`) to also carry every same-id observer `Arc` (a `Vec`, not a single slot —
subscribe delivery is first-registration-wins, so a single-Arc value would retain
only the inert duplicate, not the delivering observer); (ii) cross-task cleanup of the four
listener-lifetime maps (`mod.rs:1872-1885`) — solved by a shared retired-id set the
listener consults at each wake's init pass, mirroring the existing snapshot-then-release
pattern (`mod.rs:2038-2041`); and (iii) honest semantics for what is *not* cleaned:
checkpoint rows are retained (deletion races the lingering
`flush_all_pending_checkpoints`; orphan rows are read only by `get_checkpoint`,
`release_halt`, and a same-id re-subscribe seed), effect timing is "next wake" with a
documented current-wake tail, and the advisory lock is released best-effort with a
documented cross-session caveat. Retire is what makes the P5 cap end-state and the
readiness un-pin possible; report 3 B2 proved readiness resolvability is pure registry
membership.

**Part C — P5 as option (b): epoch retires, the application re-subscribes.** Epoch
cannot build a consumer's observer (type-erased `Arc<Mutex<dyn EventObserver<D>>>`;
"Epoch provides the trigger, not the rebuild", `config.rs:247`), so the only
race-free shape is: epoch detects the wedge via the existing set-once `GapUnproven`
fire site (`mod.rs:766-774`), retires the wedged observer through its own new API, and
invokes an application callback that owns the fresh model and its `{base}#gen{N}` id.
This makes the heal single-actor by construction (no double-healer race with an
application's existing `on_halt` logic), bounds memory (the wedged observer leaves the
registry), and unifies the ticket's two asks into one mechanism. The trigger gate is
exact — `ReplayAlways` + `FailClosed` + `GapPolicy::Halt` + `GapUnproven` — because
`SkipAfterBackstop` subscribers never form gap wedges (`subscriber_state.rs:413-427`)
and deserialize/observer wedges need operator action, not replay. Backoff defaults
mirror catacloud's converged numbers (immediate boot heal; 30 s/60 s cap re-halts) with
a `max_generations` cap whose end-state is retire + ERROR. Providing the callback hook
*is* the opt-in — "epoch healed my subscriber and I never asked" is impossible by
construction. All open questions were settled with Ruben on 2026-09-16 (§7); the
phases below implement the settled answers.

---

## 6. Codebase Map

All anchors verified at `5c27432`. `mod.rs` = `epoch_pg/src/event_bus/mod.rs` (5330
lines) unless prefixed otherwise.

### `epoch_pg/src/event_bus/mod.rs` — the delivery core

| Location | Symbol | Role in this work |
|----------|--------|-------------------|
| `mod.rs:1304` | `type Projections<D> = Arc<Mutex<Vec<Arc<Mutex<dyn EventObserver<D>>>>>>` | Push-only observer registry; unsubscribe adds the only removal (all same-id Arcs, `Arc::ptr_eq`, under this outer mutex). |
| `mod.rs:1420` | `hwm: Arc<Mutex<HashMap<String, u64>>>` | ReplayAlways in-memory HWM; entry removed at retire; reset-to-0 at re-subscribe (`mod.rs:4343-4346`). |
| `mod.rs:1429` | `subscriber_modes: Arc<Mutex<HashMap<String, SubscriptionMode>>>` | Readiness registry created because observer mutexes are held across `on_event` (doc `1416-1428`) — extended in Phase 2 to also carry every same-id observer `Arc` (id→(mode, `Vec<Arc<..>>`)); entry removal is *the* readiness switch for retire. |
| `mod.rs:1872-1885` | `checkpoint_cache` (1872), `pending_checkpoints` (1876), `subscriber_states` (1880), `last_event_ids` (1885) | Listener-task-lifetime maps (declared once, before the reconnect loop) — pruned per wake via the retired-id tombstone in Phase 3; *not* cleanable from the bus struct directly. |
| `mod.rs:527-533` | live batch row loop dedup | `event_seq <= contiguous_before` skip (529-531) and `state.processed_ahead.contains` (533) — the dedup handoff (Phase 1) is consumed here. |
| `mod.rs:766-774` | `fire_on_halt(..., HaltReason::GapUnproven)` | The only GapUnproven producer; set-once per gap via `halt_fired` — Phase 4's heal schedules from this site (spawned, never inline). |
| `mod.rs:1795-1856` | R2 startup replay pass | Re-runs `catch_up_from_checkpoint` per registered observer at listener start (call at `mod.rs:1846`) — second catch-up call site; Phase 1 handoff must cover it. |
| `mod.rs:2032-2037` | CLOUD-225 snapshot-then-release comment | "does not double-deliver" claim is hole-blind (paper 2 §4.2) — amended in Phase 1. |
| `mod.rs:2038-2041` | wake snapshot of `projections` | Snapshot-then-release (commit 2bf9512) — basis for the documented next-wake effect timing of retire. |
| `mod.rs:2063-2111` | one-time live state seed | `contains_key` gate at 2063; seeds `contiguous_checkpoint` from HWM (ReplayAlways) or checkpoint row, then `SubscriberState::new_with_event_id` (~2106) — Phase 1 consumes the delivered-above-prefix set here; Phase 3's tombstone prune/gate lives in this init pass. |
| `mod.rs:2116-2150` | P4b private wedge fetch | Intro comment (2116-2127) + ReplayAlways exclusion/remedy comment (2129-2132, contains both "a fresh `subscribe()`" and "lands in P5") + `wedged_sids` collection (2133-2139, filtered by `!replay_always_by_sid`) — P5's excluded wedge class; Phase 3 must prevent retired Checkpointed wedges from entering it. |
| `mod.rs:2206-2211` | `compute_shared_floor(subscriber_states.values())` | Shared fetch floor from non-wedged states — lingering retired-id states pin it (Phase 3 prunes); **do not change floor semantics**. (Unified anchor: the call site is 2206-2211 throughout this spec, matching R7.) |
| `mod.rs:2260-2264` | `wedged_now` set | Shared-batch exclusion of wedged subscribers (state-based, not lock-based — report 3 B3). |
| `mod.rs:2308-2327` | task-input dedup (`seen_sids`) | First-registration-wins live dispatch — why fresh `#genN` ids are mandatory for any heal. |
| `mod.rs:2498-2512` | `get_checkpoint` | Orphan-row reader (retention rationale for Phase 3). |
| `mod.rs:2594-2676` | `release_halt` | Unchanged; ReplayAlways WARN honesty at 2646-2653; `fire_on_halt(Released)` at 2667-2673 (never a heal trigger). |
| `mod.rs:2718-2732` | `subscriber_mode` | Registry-validated readiness gate; `SubscriberNotFound` at ~2730 — the observable post-retire flip. |
| `mod.rs:2952-2981` | `wait_until_all_caught_up` | Snapshots `subscriber_modes` from the registry, not the observers (2966-2972); empty registry after the snapshot → trivial `Ok(true)` at the check (2974-2976) — hazard to document at retire. |
| `mod.rs:2999-3041` | `fast_forward_all_subscribers` | Snapshot at 3022; identifies observers by locking each one's inner mutex (3025-3033) — pre-existing scan that blocks behind in-flight `on_event` handlers, retrofitted to the Phase 2 registry. |
| `mod.rs:3646` | inline drain per-entry snapshot | Inline dispatch re-snapshots per queue entry — retire effective on the next drained event; test target. |
| `mod.rs:3446-3470` | `resolve_gap_timeout` | Existing resolve-by-row-id helper — pattern for the `resolved_by='unsubscribe'` bulk resolve. |
| `mod.rs:447-468` | scan resolve (`'gap_detection'`) | The resolve-once UPDATE pattern (full unique-key WHERE) — Phase 3's ledger resolve mirrors it keyed on `(bus_name, subscriber_id)`. |
| `mod.rs:3477-3496` | `try_acquire_subscriber_lock` | Session-scoped `pg_try_advisory_lock` via pooled connection (Coordinated subscribe). |
| `mod.rs:3499-3512` | `release_subscriber_lock` | Zero callers today; cross-session unlock returns `false` — best-effort call at retire, documented unreliable. |
| `mod.rs:3758-3775` | `warn_if_subscriber_id_reused` | First-registration-wins: inserts/overwrites the mode mapping and warns — stays as-is; retire makes same-id re-subscribe a clean lifecycle. |
| `mod.rs:3798-3825` | `advance_catchup_prefix` | CLOUD-227 unbroken-prefix guard (`event_global_seq != *contiguous + 1 → return`, ~3817-3819) — Phase 1 must keep the position pinned at the hole. |
| `mod.rs:3892-4163` | `catch_up_from_checkpoint` | Delivers every scanned row once (`mod.rs:4069` region); pagination cursor advances past holes (`current_sequence = event_global_seq`, 4105) — Phase 1 extends its return to expose the delivered-above-prefix set. |
| `mod.rs:4228-4247` | `SkipAfterBackstop` guardrail | Registration-time validation pattern (`InvalidSubscriptionConfig` before any side effect) — model for any new registration checks. |
| `mod.rs:4259-4260` | inline subscribe push | `guard.push(observer)` — one of the two registry write sites Phase 2 must mirror. |
| `mod.rs:4329-4346` | subscribe HWM reset | ReplayAlways HWM zeroed before catch-up (4343-4346) — "fresh model ⇒ replay from 0" contract reused by heal semantics. |
| `mod.rs:4439-4448` | subscribe's `catch_up_from_checkpoint` call | Phase 1 handoff site (delivered set captured here). |
| `mod.rs:4477-4505` | subscribe's buffer-drain leg | Cursor-deduped continuation of catch-up (`global_sequence > current_sequence`); rows it delivers above the hole share the same exposed window — Phase 1 handoff covers it. |
| `mod.rs:1231` | `PgEventBusError::SubscriberNotFound(String)` | Existing variant reused for the post-retire readiness flip. |

### `epoch_pg/src/event_bus/subscriber_state.rs` — the gap-aware resolver

| Location | Symbol | Role in this work |
|----------|--------|-------------------|
| `subscriber_state.rs:75-78` | `GapObservation.halt_fired` doc | Set-once, halt-entry-only — the exactly-once-per-(generation, gap) heal trigger guarantee. |
| `subscriber_state.rs:170-186` | `SubscriberState::new_with_event_id` | The single production seeding constructor; `processed_ahead: HashSet::new()` born empty — Phase 1 adds the delivered-above-prefix set as a seed parameter. |
| `subscriber_state.rs:199-202` | `is_wedged` | `FailClosed ∧ (held_event ∨ any halt_fired)` — the wedge definition driving both fetch exclusions and the heal gate. |
| `subscriber_state.rs:413-437` | resolver arms | `SkipAfterBackstop` arm (413-427) mutates no state (never gap-wedges); `FailClosed`+`Halt` arm (428-437) sets `halt_fired` + `backstop_refused` — the conjunctive P5 gate basis. **Do not modify.** |

### `epoch_pg/src/event_bus/retry.rs` and `config.rs`

| Location | Symbol | Role in this work |
|----------|--------|-------------------|
| `retry.rs:43-60` | `invoke_observer_once` | Holds the observer mutex across the whole `on_event` await — the reason identification must be lock-free (Phase 2). |
| `retry.rs:142-152` | `process_event_with_retry` | Retry ladder holding `&Arc<Mutex<dyn EventObserver>>` throughout. |
| `config.rs:59-60` | `HaltReason` (`#[non_exhaustive]`) | Phase 4 may add a variant (e.g. `WedgeSuperseded`) source-compatibly. |
| `config.rs:247-258` | `RebuildNeededCallback` doc (quote at 247; "Who receives it"/"Blocking contract" through 258) | "Epoch provides the *trigger*, not the rebuild" — the contract the Phase 4 callback docs mirror. |
| `config.rs:~500`, `~519` | `on_rebuild_needed`, `gap_scan_interval` | Field/Default patterns for `on_wedge_retired` + `wedge_heal` policy (Phase 4). Default impl at ~524-543 (`gap_timeout` 5 s, `snapshot_fencing` true). |

### Other crates, migrations, tests

| Location | Symbol | Role in this work |
|----------|--------|-------------------|
| `epoch_core/src/event_store.rs:231`, `:247` | `pub trait EventBus`, `subscribe` | Phase 5 adds `unsubscribe` (settled: defaulted method, default body `Ok(false)` — non-breaking). |
| `epoch_mem/src/event_store.rs:557` | `projections: Arc<RwLock<Vec<Box<dyn EventObserver<D>>>>>` | Parity target: `Box` in `RwLock<Vec>`, read lock per event (~618-624), push-only subscribe (755-768); removal under the write lock is a plain retain. |
| `epoch_pg/src/migrations/m009_create_gap_timeout_log.rs:35-48`, `:43` | `epoch_event_bus_gap_timeouts` (+`resolved_by`) | Ledger resolve target; no new migration. |
| `epoch_pg/src/migrations/m003_create_event_bus_infrastructure.rs` | checkpoints/DLQ tables | Retained at retire (no DELETE is added anywhere). |
| `epoch_pg/tests/pgeventbus_integration_tests.rs:5747` | `isolated_events_table(pool)` | All new DB-gated integration tests live in this file, `#[serial]`, per-test isolated tables (158 total `fn` definitions in the file; 127 are `#[tokio::test]` test functions and set the naming style; 131 carry `#[serial]`). |
| `epoch_pg/tests/common/mod.rs:157-240` | `try_get_pg_pool` / `EPOCH_REQUIRE_DB` gating | Suite convention: skip-without-DB unless `EPOCH_REQUIRE_DB=1`. |
| `CHANGELOG.md` (Unreleased) | — | Phase 5 records the new public surface. |

### Load-bearing constraints

- **Do not** make `catch_up_from_checkpoint` stop delivering above a hole, and **do
  not** move the live fetch floor/cursor past a hole (`mod.rs:2206-2211`,
  `mod.rs:3892-4163`) — both break fail-open/Checkpointed backlog delivery and
  CLOUD-227/gap detection (report 4 §Implications 4).
- **Do not** modify the resolver's `SkipAfterBackstop` arm (`subscriber_state.rs:413-427`)
  or `release_halt` (`mod.rs:2594-2676`) — spec 0030 surface.
- **Do not** add a migration; the ledger already has `resolved_by` (`m009:43`).
- **Do not** let any heal path run inline in `fire_on_halt` — the callback is awaited
  on the bus's batch loop (paper 2 §3.3, catacloud spawns for exactly this reason).
- Repo conventions: Conventional Commits (scopes `pg`/`core`/`mem` + `event-bus`),
  `cargo fmt`, zero clippy warnings, rustdoc on public APIs, no new dependencies.

---

## 7. Decisions

All eleven open questions were resolved with Ruben in a decision interview on
2026-09-16. OQ-1, OQ-2, OQ-5, OQ-6, and OQ-9 were decided directly; OQ-3, OQ-4,
OQ-8, OQ-10, and OQ-11 accepted the spec's standing recommendations; OQ-7 was ruled
out of epoch's scope. The requirements and phases above already implement these
answers; this section is the record.

- **OQ-1 — in scope.** The dedup fix ships as Phase 1 of this spec (the
  "prerequisite ticket" alternative is retired; Phase 4 depends on Phase 1, not on an
  external ticket).
- **OQ-2 — idempotent.** `unsubscribe` returns `Result<bool, PgEventBusError>`:
  `Ok(true)` removed, `Ok(false)` unknown id. Readiness methods keep
  `SubscriberNotFound`; the asymmetry is intentional (query vs teardown).
- **OQ-3 — deferred.** No retire-by-prefix in 0031; exact-id composes with the
  `#genN` scheme. A family-match wrapper remains a thin future addition over the
  exact-id primitive.
- **OQ-4 — catacloud-mirroring defaults + cap.** The shipped default schedule is
  immediate on a boot-generation wedge, 30 s then 60 s cap on heal-generation
  re-halts; config-overridable; plus a `max_generations` cap (catacloud has none)
  whose end-state is retire-the-chain + ERROR + no further heals.
- **OQ-5 — opt-in by construction.** Registering the heal callback *is* the opt-in;
  no extra flag; `GapPolicy::Halt` behaviour is byte-for-byte unchanged without it.
  Precisely: the heal is active iff `on_wedge_retired` is `Some`; `wedge_heal: None`
  with a callback configured means the default backoff schedule applies — there is no
  separate enable flag beyond the callback's presence.
- **OQ-6 — defaulted trait method.** `EventBus::unsubscribe` is added as a defaulted
  method; the default body returns `Ok(false)` because `Self::Error` has no
  construction bound (an `Err(Unsupported)` default would require a breaking bound);
  `InMemoryEventBus` overrides (~10 lines). Non-breaking for downstream implementors;
  the generation-chain consumer story stays portable to in-memory rigs.
- **OQ-7 — out of epoch's scope.** Catacloud's adoption timing (subsume-and-delete
  vs soak) is catacloud's call, not an epoch decision; dropped from this spec's
  decision list. The epoch-side coordination item stands unchanged: **ping
  CLOUD-259 when spec 0031 lands**, and hand over paper 2 §8.2's two interim guards
  for any window where both healers coexist.
- **OQ-8 — retain.** Checkpoint and DLQ rows are retained on retire
  (`flush_all_pending_checkpoints` can resurrect a deleted row; orphan rows are read
  only by `get_checkpoint` (`mod.rs:2498`), `release_halt` (`mod.rs:2599`), and a
  same-id re-subscribe seed); gap-ledger rows resolve with
  `resolved_by='unsubscribe'`.
- **OQ-9 — option (b).** Epoch retires the wedged observer and invokes the
  application heal callback; epoch never constructs or `subscribe()`s observers.
  Option (a) factory heal stays a possible future spec reusing the same trigger.
- **OQ-10 — document now.** The empty-registry `wait_until_all_caught_up → Ok(true)`
  hazard is documented in R9; a guard is a small follow-up if ever wanted.
- **OQ-11 — defer.** No advisory-lock ownership fix in 0031; R10 documents the
  best-effort release reality.

---

## 8. Risks & Mitigations

| Risk | Likelihood | Mitigation |
|------|------------|------------|
| Dedup fix implemented in a forbidden shape (stop delivering above holes; move the fetch cursor) silently strands fail-open/Checkpointed backlogs or breaks CLOUD-227 | Low | R3 non-regression pins; forbidden shapes named in Phase 1 scope; report 4 §Implications 4 documents why each is wrong. |
| Retire timing races (current-wake tail keeps driving/flushing the retired observer; same-id re-subscribe inert for one wake) surprise consumers | Med | R9 mandates documented next-wake semantics; Phase 3 tests pin the one-wake window; at-least-once semantics tolerate the tail. |
| Double-healer race during the catacloud transition (both catacloud Phase 3 and epoch P5 react to the same halt) | Med | Option (b) is single-actor by construction *within* epoch; if catacloud ever runs both healers it needs the two guards (paper 2 §8.2) — coordination item on CLOUD-259 (OQ-7 settled out of epoch's scope; adoption timing is catacloud's); P5 is opt-in so an unconfigured bus never heals. |
| Retiring the last subscriber makes `wait_until_all_caught_up` trivially `Ok(true)` (monitoring blind spot) | Low | R9 documents the hazard; settled: documentation now, a guard remains a possible follow-up. |
| Advisory-lock release silently fails (cross-session) leaving a Coordinated lock held until session death | Med | R10: best-effort attempt + explicit rustdoc that locks die with the pooled session; settled: ownership fix deferred. |
| Zombie rebuild callbacks for retired ids confuse rebuild automation | Low | R8 resolves retired ids' ledger rows with `resolved_by='unsubscribe'`; test in Phase 3. |
| Anchor drift between spec and implementation (mod.rs is 5330 lines and actively edited) | Med | Every anchor pairs `path:line` with the symbol name and a re-location hint; phases instruct searching by symbol first. |
| Heal backoff tests are time-dependent and flaky | Low | R14 requires the schedule to be config-injectable; tests use tiny intervals (the suite's `gap_timeout: 500ms` pattern from report 4). |

---

## 9. Delivery Plan

Five phases in three parts. Phase 1 (Part A) and Phase 2 (Part B prerequisite) are
independent and may run in parallel. Phase 3 needs Phase 2; Phase 4 needs Phase 1 and
Phase 3; Phase 5 closes out. All phases are
TDD-oriented: write the failing DB-gated integration test first (where practical),
then implement to green, matching how spec 0030's phases were executed (tests in
`epoch_pg/tests/pgeventbus_integration_tests.rs`, `#[serial]`, `isolated_events_table`
(`:5747`), `EPOCH_REQUIRE_DB=1`, relative sequence assertions only).

### Phase 1: Exactly-once fresh subscribe over open holes (Part A)

- **Goal**: A fresh `ReplayAlways` `subscribe()` over a burned hole delivers every
  above-hole row exactly once (counts `(3,1),(4,1)` — today `(3,2),(4,2)`, report 4
  B1) while its position stays pinned below the hole; the same holds across process
  boots and listener restarts.
- **Requirements Covered**: R1, R2, R3
- **Scope**:
  - Modify `epoch_pg/src/event_bus/subscriber_state.rs:170-186`
    (`SubscriberState::new_with_event_id`) — accept the **exact set of sequences
    delivered above the pinned prefix** (a set or coalesced-interval structure
    alongside `processed_ahead`; implementer's choice, but never an upper bound
    alone — see the rejected range-watermark alternative below).
  - Modify `epoch_pg/src/event_bus/mod.rs:3892-4163` (`catch_up_from_checkpoint`) —
    extend the `Result<(u64, u64), SqlxError>` return to also expose the exact set of
    sequences delivered while the prefix pinned at the hole. Do **not** change what
    it delivers.
  - Modify `mod.rs:4439-4448` (subscribe's catch-up call) and the buffer-drain leg
    (`mod.rs:4477-4505`) — capture the delivered set and hand it to the shared state
    the listener seed reads (a small shared map beside `hwm`/`subscriber_modes` is
    the natural carrier; exact carrier is the implementer's choice within the
    contract).
  - Modify `mod.rs:1846-1856` (R2 startup replay pass call site) — same handoff so
    listener restarts are covered (R2).
  - Modify the one-time live state seed `mod.rs:2063-2111` (init pass,
    `contains_key` gate at 2063) — consume the delivered set when constructing
    `SubscriberState` for the fresh id.
  - Modify the dedup consumption point `mod.rs:527-533` — the delivered set is
    consulted beside `processed_ahead.contains`.
  - **Rejected alternative**: an O(1) range watermark (single max-sequence-delivered
    value) instead of the exact set. Catch-up only sees currently-visible rows, and
    sequences can materialize late (`scan_late_materialized_gaps`, `mod.rs:394`; gap
    machinery `subscriber_state.rs:410-437`). Repro: burn seq 2, hold seq 4 open in an
    uncommitted txn, publish 3/5/6, subscribe (catch-up delivers 3, 5, 6; a range
    watermark would be set to 6), commit seq 4 — the first live wake finds `4 ≤ 6`
    and skips it forever, a silent loss. The contract is: the dedup state must record
    exactly the sequences delivered above the prefix — never an upper bound alone.
  - Amend the hole-blind comment `mod.rs:2032-2039` (CLOUD-225 "does not
    double-deliver") to state the now-true behaviour.
  - Document the amended R2 contract in rustdoc on the handoff carrier
    (`pending_delivered_sets`) and `CatchUpOutcome`: catch-up passes remain
    at-least-once per pass while the prefix stays pinned; the handoff guarantees only
    that the live wake loop adds no further copies; a startup catch-up that errors
    mid-pass drops its partial handoff and the live pass re-delivers that partial
    batch (at-least-once). (Added 2026-09-17, Phase 1 review cycle 1.)
  - Create tests in `epoch_pg/tests/pgeventbus_integration_tests.rs` (pattern: the
    CLOUD-261 wedge recipes from report 4's Method section — `isolated_events_table`,
    burn via claim-in-open-tx-then-rollback, `snapshot_fencing: false`, tiny
    `gap_timeout`): failing-test-first `test_fresh_subscribe_over_open_hole_delivers_exactly_once`,
    `test_listener_restart_over_open_burn_live_pass_adds_no_copies` (R2 pass leg;
    pins amended R2 — the live pass adds no copies, catch-up passes remain
    at-least-once per pass),
    `test_fresh_subscribe_hole_free_stream_unchanged` and
    `test_fresh_subscribe_position_still_pins_at_hole` (R3 pins),
    `test_fresh_subscribe_late_materialized_row_inside_catchup_range_delivered_once`
    (hold a mid-range sequence in an open txn, burn the hole, publish rows above it,
    run the fresh subscribe so catch-up delivers what's currently visible, commit the
    held row, then confirm one live wake delivers it exactly once — the pin that kills
    the rejected range-watermark shape), plus the `Checkpointed`-variant confirm leg
    recommended by report 4's OQ-2.
  - Explicitly out of bounds: no change to what `catch_up_from_checkpoint` delivers;
    no change to `compute_shared_floor` (`mod.rs:2206-2211`) or the fetch cursor; no
    `SkipAfterBackstop`/resolver changes (`subscriber_state.rs:413-437`); no
    migration; no retire/heal work.
- **Entry Conditions**: Baseline rev `5c27432` with the four verified probe reports
  available in `docs/probes/`. No phase dependencies.
- **Exit Criteria / Verifiable Artifacts**: The five new tests plus the `Checkpointed`
  confirm leg pass under `EPOCH_REQUIRE_DB=1`; the pre-existing CLOUD-261/0030 Part A suite stays green
  (including the `Halt` default pins); `cargo clippy -- -D warnings` clean; commits
  use `fix(pg)`/`test(pg)` Conventional Commits.
- **Parallelism**: PARALLEL with Phase 2 (disjoint code regions: catch-up/seed vs
  registry declarations; no semantic dependency).
- **Relative Effort**: S — one delivered-set handoff plus two seed/consume sites and
  tests; the probe recipes for the failing test are already documented.
- **Difficulty**: `hard` — correctness-critical change in shared delivery control
  flow where two of the three plausible fix shapes are silently catastrophic
  (stranded backlogs, broken gap detection).
- **Open Questions / Blockers**: None. OQ-1 settled 2026-09-16: the dedup fix ships
  in-scope as this phase; Phase 4 depends on it directly.

### Phase 2: Lock-free id→observer registry (Part B prerequisite)

- **Goal**: The bus can identify any registered observer by subscriber id without
  acquiring its mutex — `fast_forward_all_subscribers` completes while a wedged
  handler holds its observer's mutex for the whole test.
- **Requirements Covered**: R5
- **Scope**:
  - Modify `mod.rs:1429` (`subscriber_modes` field + its rustdoc `1416-1428`) —
    extend the registry value to carry **every** same-id observer handle, e.g.
    `HashMap<String, (SubscriptionMode, Vec<Arc<Mutex<dyn EventObserver<D>>>>)>` (paper
    1 V1/§4.4; a single-Arc value was considered and rejected — `subscribe()` pushes
    unconditionally at the two push sites below and delivery is first-registration-
    wins via `seen_sids` (`mod.rs:2314-2321`), so an overwriting single-Arc map would
    retain only the *last* registered Arc — the inert duplicate, not the delivering
    observer — making R6's "remove all same-id Arcs" unachievable). Update all
    readers/writers: the two subscribe push sites `mod.rs:4259-4260` (inline) and
    `mod.rs:4731-4732` (async) append to the id's `Vec` rather than overwrite;
    `warn_if_subscriber_id_reused` (`mod.rs:3758-3775`, which owns the map insert)
    pushes the new Arc onto the existing entry's `Vec` while still warning on reuse;
    `subscriber_mode` (`mod.rs:2718-2732`); the readiness snapshot
    (`mod.rs:2966-2972`); `release_halt`'s mode lookup (`mod.rs:2640-2645`) all read
    the mode off the entry (mode is per-id, not per-Arc, so it does not need to move
    into the `Vec`).
  - Modify `fast_forward_all_subscribers` (`mod.rs:2999-3041`; observer-lock scan at
    `3025-3033`) — resolve mode/id from the registry instead of locking each
    observer; the `projections` Vec snapshot stays for the update path.
  - Add the `Arc::ptr_eq`-based removal helper for the projections Vec as an internal
    fn near the registry — written now, exercised by Phase 3's `unsubscribe` (a unit
    test may pin it here). **Capture-based contract, not lookup-based**: the helper
    takes an already-captured `Vec<Arc<Mutex<dyn EventObserver<D>>>>` (the caller
    clones the id's Arcs out of the registry entry under the outer mutex *before*
    dropping that entry) and removes every Arc in the captured `Vec` from the
    projections Vec by `Arc::ptr_eq`, under the outer mutex. The helper does **not**
    look the id up in the registry and does **not** drop the registry entry itself —
    both are the caller's responsibility, in that order (capture, then drop the
    entry), because a caller that drops the registry entry first leaves nothing for
    an internal lookup to find (Phase 3's removal order reflects this).
  - Create tests in `epoch_pg/tests/pgeventbus_integration_tests.rs`:
    `test_fast_forward_completes_while_observer_mutex_held` (observer whose
    `on_event` parks on a channel; assert `fast_forward_all_subscribers` returns),
    plus a registry-shape unit test.
  - Explicitly out of bounds: no public `unsubscribe` yet (Phase 3); no listener-map
    or tombstone changes; no readiness semantic changes (the registry extension is
    behaviour-preserving for readiness).
- **Entry Conditions**: None beyond the baseline (parallel-safe with Phase 1; if both
  run concurrently, Phase 2 lands second or rebases on Phase 1's mod.rs — the touched
  regions are disjoint).
- **Exit Criteria / Verifiable Artifacts**: New tests pass; full DB-gated suite green;
  clippy clean; `projections.lock()` call-site inventory (six sites:
  `mod.rs:1813, 2039, 3022, 3646, 4259, 4731`) still complete — no additional write
  path introduced silently.
- **Parallelism**: PARALLEL with Phase 1 (disjoint regions; shared file only).
- **Relative Effort**: S — one map type change with a bounded reader/writer set and a
  mechanical retrofit.
- **Difficulty**: `standard` — concurrency-adjacent but mechanically small; the
  pattern (never lock an observer to read metadata) is already established by the
  field's own rustdoc.
- **Open Questions / Blockers**: None identified. (Paper 1's OQ-6 "registry redesign
  in 0031?" is answered here by the discovery's finding 4: identification without
  locking is a hard prerequisite for "safe on wedged subscribers".)

### Phase 3: `unsubscribe` retire API with full removal inventory (Part B)

- **Goal**: `unsubscribe(id)` retires a wedged or idle subscriber end-to-end on a live
  bus: `wait_until_all_caught_up` un-pins and resolves, per-id readiness flips to
  `SubscriberNotFound`, ledger rows resolve as `'unsubscribe'`, checkpoint/DLQ rows
  are retained, and no listener residue pins the floor.
- **Requirements Covered**: R4, R6, R7, R8, R9, R10
- **Scope**:
  - Add `pub async fn unsubscribe(&self, subscriber_id: &str) -> Result<bool,
    PgEventBusError>` on `PgEventBus` (placement near the readiness/subscription
    methods; rustdoc carries the R8/R9 contracts). Removal order, **capture before
    drop** (the Phase 2 `ptr_eq` helper is capture-based, not lookup-based — it
    cannot find an id's Arcs once the registry entry is gone):
    (a) capture the id's `Vec<Arc<..>>` by cloning it out of the `subscriber_modes`
    registry entry, under the outer mutex; (b) remove every captured Arc from the
    projections Vec via the Phase 2 `ptr_eq` helper (all same-id Arcs, including the
    delivering one, not just the last-registered duplicate); (c) drop the
    `subscriber_modes` entry — readiness flips here; (d) `hwm` entry out
    (`mod.rs:1420`) **and** the id's `pending_delivered_sets` entry out (the Phase 1
    delivered-set handoff carrier — removing it here makes every still-present entry
    provably fresh, recorded by a post-(d) subscribe; a stale unconsumed set inherited
    by a re-seeded lifecycle would silently suppress deliveries it never received. Do
    **not** also re-clear this map in the init-pass prune (i): a same-id re-subscribe
    that recorded a fresh set between step (d) and the consuming wake must keep it —
    over-pruning only degrades that interleaving to at-least-once, never suppresses);
    (e) unresolved `epoch_event_bus_gap_timeouts` rows for
    `(bus_name, subscriber_id)` resolved with `resolved_by='unsubscribe'` (one
    UPDATE, mirroring `mod.rs:447-468` / `resolve_gap_timeout` `mod.rs:3446`);
    (f) Coordinated mode: best-effort `release_subscriber_lock`
    (`mod.rs:3499-3512`) with the unreliability documented; (g) checkpoint and DLQ
    rows retained — no DELETE added; (h) insert the id into the retired-id tombstone
    (below) as the final step, so a wake racing steps (a)-(g) still finds a
    consistent registry state (tombstone absent) and only the *next* wake sees the
    retirement.
  - Add the cross-task retired-id tombstone as a **pending-prune marker consumed by
    the listener's wake init pass**, not a set `subscribe()` ever touches. Shared set
    beside the registry (e.g. `retired_ids: Arc<Mutex<HashSet<String>>>`, declared at
    `mod.rs:1429` region); `unsubscribe` inserts the id (step (h) above) **only when a
    listener is running** — `DispatchMode::Inline` buses have no listener
    (`start_listener` is a no-op there, `mod.rs:1740-1745`) and therefore no
    listener-lifetime maps to prune, so `unsubscribe` on an Inline bus stops after
    step (g) and never inserts a marker. On a bus with a running listener, at each
    wake's init pass (`mod.rs:2063` region), **before** the `contains_key` gate: for
    every id present in `retired_ids`, (i) prune its entries from the four
    listener-lifetime maps (`mod.rs:1872-1885`) **and** remove its `hwm` entry
    (`mod.rs:1420`) — the ReplayAlways advance path can re-insert `hwm[id]` after
    step (d) already removed it (`mod.rs:3821-3823`), so the prune must re-clear it
    or a retired `ReplayAlways` id leaves a permanent orphan `hwm` entry,
    accumulating across a `#genN` heal chain; (ii) **fence that id's pre-unsubscribe
    snapshotted observer `Arc`** (the wake's own snapshot, `mod.rs:2038-2041`)
    **from this wake's dispatch**, whether or not the id is currently registered
    again — the snapshot was taken before the unsubscribe, so its Arc for that id is
    necessarily the retired one; a re-registered replacement's Arc was pushed after
    the snapshot and joins delivery from the next wake, consistent with R9's "inert
    for up to one wake"; and then (iii) **drop the id from `retired_ids`** — the
    marker is consumed, not left standing. The fence is load-bearing, not cosmetic:
    without it, (a) a pruned-and-not-re-registered id's stale Arc would still be
    dispatched to on this wake, and the per-priority dispatch's
    `subscriber_states.remove(sid).unwrap_or_else(|| SubscriberState::new(0))`
    fallback (`mod.rs:2328-2330`) — normally unreachable because the init pass seeds
    every subscriber this cycle (comment at `mod.rs:2323-2327`) — would silently
    substitute FailOpen+Halt defaults for what was a FailClosed subscriber; and (b) a
    pruned-and-re-registered id's stale Arc would consume the new lifecycle's
    replay-from-0 and advance its position (`subscribe()` zeroes the ReplayAlways
    HWM at `mod.rs:4343-4345`). The `contains_key` gate then runs as normal and
    re-seeds the id **if and only if** a lock-free registry lookup shows it is
    currently registered again (a same-id re-subscribe landed); if it is not
    currently registered, the id simply stays pruned (no re-seed). `subscribe()`
    never reads or writes `retired_ids` — this is what makes the design race-free in
    both orderings: **(retire → wake → re-subscribe)**: the wake prunes the old
    state, fences the stale Arc, and finds nothing re-registered yet, so it drops
    the marker and re-seeds nothing; the re-subscribe that follows seeds a fresh
    lifecycle through the normal `contains_key` gate, with no stale state left to
    inherit. **(retire → re-subscribe → wake)**: the marker is still present when
    the wake runs, so the prune and fence still execute first (dropping and
    excluding any stale state a fast re-subscribe might otherwise have raced
    against), and the `contains_key` gate's registry lookup then finds the id
    registered and re-seeds it fresh from the next wake on. In both orderings the
    fresh seed observes empty listener state — R8's clean-lifecycle contract
    (`Checkpointed` resumes from the retained checkpoint row; `ReplayAlways` replays
    from 0; no first-registration-wins warning) holds unconditionally, and R9's
    "inert for up to one wake" window is preserved (not permanent). On a bus with a
    running listener, the set stays bounded: every entry is consumed by the next
    wake that observes it, regardless of whether a re-subscribe happened.
    (Recommended mechanism per paper 2 §5.3 / report 3 OQ-1; a channel alternative
    was considered and set aside.) This also removes floor participation
    (`mod.rs:2206-2211`) and P4b private-fetch participation (`mod.rs:2133-2150`)
    from the next wake on.
  - **Subscribe/retire race** (the invariant the id→Arc registry (Phase 2) must
    uphold once `unsubscribe` exists to race against it): the async subscribe path
    has a multi-step window (registry insert via `warn_if_subscriber_id_reused`,
    `mod.rs:3758-3775`; catch-up runs; projections push at `mod.rs:4731-4732`)
    during which a concurrent `unsubscribe` of the same id can land. Invariant: **an
    observer Arc in the projections Vec always has a live registry entry for its
    id.** Each push site re-verifies, in the same outer-mutex critical section as
    the push (or an immediate re-check), that the id is still present in
    `subscriber_modes` after pushing; if a concurrent `unsubscribe` retired the id
    mid-subscribe, the push site removes the Arc it just pushed and aborts the
    subscription by returning `Err(SubscriberNotFound)` — `unsubscribe` wins over a
    racing `subscribe()` of the same id (R9's timing contract; the caller may
    re-issue the subscribe if the subscription is still wanted).
  - Inline dispatch: verify the per-entry snapshot (`mod.rs:3646`) picks up removal on
    the next drained event — behaviour test, no structural change expected.
  - Create tests in `epoch_pg/tests/pgeventbus_integration_tests.rs`:
    `test_unsubscribe_removes_registered_subscriber_returns_true` and
    `test_unsubscribe_unknown_id_is_idempotent` (R4; settled `Ok(bool)` contract),
    `test_unsubscribe_wedged_subscriber_restores_all_caught_up_gate` (report 3 B2
    turned into a test: wedged ReplayAlways+Halt registered → gate false 3 rounds;
    unsubscribe → gate resolves), `test_retired_id_readiness_returns_subscriber_not_found`
    (R6), `test_retired_id_does_not_pin_shared_floor` (R7), and
    `test_unsubscribe_resolves_gap_ledger_rows_with_unsubscribe_marker` +
    `test_unsubscribe_retains_checkpoint_row_and_resubscribe_resumes` (R8),
    `test_unsubscribe_inline_bus` (R4), `test_unsubscribe_effect_at_next_wake` (R9
    timing pin), `test_unsubscribe_coordinated_best_effort_lock_release` (R10), and
    `test_resubscribe_same_id_before_next_wake_starts_clean` (the tombstone race:
    unsubscribe(id) then immediately re-subscribe the same id before any further
    wake runs; a `ReplayAlways` re-subscribe replays from 0, a `Checkpointed`
    re-subscribe resumes from the retained checkpoint row, and no retired state
    leaks — asserted via observable readiness/delivery, not internals: no inherited
    halt, no inherited gap. **Determinism note**: the listener wakes on both NOTIFY
    and the ~1 s `flush_interval` timer (`mod.rs:1888`), so the test must publish
    nothing between the unsubscribe and the re-subscribe, and its assertions must
    tolerate a wake landing inside that window — or drive a wake explicitly — to
    stay deterministic under `#[serial]` DB load).
  - Explicitly out of bounds: no heal/P5 logic; no `release_halt` changes; no
    checkpoint deletion; no retire-by-prefix (settled: deferred — a family-match
    wrapper over this primitive remains a thin future addition); no epoch_mem work
    (Phase 5).
- **Entry Conditions**: Phase 2 complete — the id→Arc registry and `ptr_eq` removal
  helper exist (this phase identifies observers through them; a Vec-scan
  implementation that locks observers is rejected by R5).
- **Exit Criteria / Verifiable Artifacts**: All new tests pass under
  `EPOCH_REQUIRE_DB=1`; full suite green (readiness, P4b, gap-scan suites unaffected
  for non-retired ids); clippy clean; rustdoc on the new public method carries the
  timing, retention, and empty-registry-hazard contracts.
- **Parallelism**: SEQUENTIAL after Phase 2 (needs the registry). Independent of
  Phase 1 — may run in parallel with it if Phase 2 has landed, but the default
  sequence is 1 → 2 → 3 in one worktree.
- **Relative Effort**: M — many small surfaces (registry, Vec, hwm, tombstone, ledger
  SQL, lock release) plus an eleven-test battery; each is individually simple but the
  inventory must be complete.
- **Difficulty**: `hard` — cross-task state cleanup with timing semantics (current
  wake vs next wake), multi-surface removal ordering, and concurrency-sensitive
  `Arc::ptr_eq` removal under the outer mutex.
- **Open Questions / Blockers**: None — all settled 2026-09-16 (OQ-2 `Ok(bool)`,
  OQ-3 deferred, OQ-8 retain, OQ-10 documented).

### Phase 4: P5 wedge heal — retire-and-notify with fresh generations (Part C)

- **Goal**: A `ReplayAlways`+`FailClosed`+`Halt` subscriber wedged by `GapUnproven`
  is retired exactly once and the application heal callback is invoked with a fresh
  `{base}#genN` id under the configured backoff; the healed replay delivers exactly
  once (Phase 1's property holds across the heal); `SkipAfterBackstop` and
  deserialize/observer wedges never trigger it; the generation cap retires the family
  with an ERROR and stops.
- **Requirements Covered**: R12, R13, R14, R15
- **Scope**:
  - Modify `epoch_pg/src/event_bus/config.rs` — add `on_wedge_retired:
    Option<Arc<dyn WedgeRetiredCallback>>` (new callback trait; docs modelled on
    `RebuildNeededCallback`'s "Epoch provides the trigger, not the rebuild" contract,
    `config.rs:247-258`) and `wedge_heal: Option<WedgeHealPolicy>` with
    `max_generations: u32` and a config-injectable backoff schedule (default:
    immediate boot-generation heal; 30 s then 60 s cap on heal-generation re-halts —
    settled defaults). Both default `None` (opt-in by construction, settled).
    Optionally add a `HaltReason` variant (e.g. `WedgeSuperseded`) to the
    `#[non_exhaustive]` enum (`config.rs:59-60`).
  - Modify `mod.rs:766-774` (`fire_on_halt(GapUnproven)` site, inside the free fn
    `process_subscriber_for_batch`, `mod.rs:474-494`, which has no bus handle,
    registry access, tombstone, or family counter in scope) — this site only
    **gate-checks and emits**; it does no unsubscribe/mint/callback work itself. The
    gate resolves from the `SubscriberState` already in scope at the fire site
    (`state.failure_mode` / `state.gap_policy`, fields at
    `subscriber_state.rs:121,133`, the same fields already read at `mod.rs:709-710`;
    `replay_always` is in scope at `mod.rs:497-498`) — the gate introduces **no new
    observer locking**: `replay_always` reuses the wake's pre-existing
    `projection.lock().await.subscription_mode()` read at that site (`mod.rs:497-498`),
    the same lock every wake already takes, not a lock added for this gate; the R5
    pattern it must not repeat is the *unbounded* scan `fast_forward_all_subscribers`
    does today (locking every observer to find one by id), which this gate never does.
    When the gate matches (`ReplayAlways` + `FailClosed` + `GapPolicy::Halt` +
    `GapUnproven`) and a callback is configured: emit a
    `HealRequest { subscriber_id, held_below_sequence }` over a new
    `Option<mpsc::UnboundedSender<HealRequest>>` field added to `BatchContext`
    (`mod.rs:70-85`) — `None` when no callback is configured, so the unconfigured
    path costs nothing (opt-in by construction). The once-per-(generation, gap)
    property is already guaranteed upstream, not by this fire site: the resolver sets
    `halt_fired` once, on the false→true transition, at
    `subscriber_state.rs:433-435`, strictly before `outcome.backstop_refused` reaches
    this fire site at `mod.rs:766-768`; the fire site does not and cannot set
    `halt_fired` itself.
  - Add a single bus-owned **heal-actor task** receiving `HealRequest`s over the
    channel's paired `mpsc::UnboundedReceiver`. The actor owns everything the fire
    site cannot reach: the per-family in-memory monotonic generation counters
    (family match: exact base OR `base#gen<digits>`, with near-miss rejection —
    semantics pinned by catacloud's `healed_subscriber_id_suffixes_generation`) and
    the backoff schedule. Per request it (i) mints the fresh id `{base}#gen{N}`,
    (ii) calls `unsubscribe` on the wedged id, (iii) invokes the callback with (base,
    wedged id, generation, held-below sequence), (iv) WARNs once per heal (old id,
    new id, generation, held-below) and ERRORs at the `max_generations` cap with no
    further heals. This satisfies R14's "never inline in the halt callback" by
    construction — the fire site never awaits the heal work, only a channel send.
    Idempotency across gaps is per-family, mirroring catacloud's `retry_pending`
    swap.
  - **Lifecycle** (the actor must not outlive the bus it heals, or a detached task
    survives `shutdown()` and leaks into the next `#[serial]` DB test): (a) **spawn
    site** — the heal-actor task is spawned inside `start_listener`
    (`mod.rs:1735`, alongside the existing `tokio::spawn` at `mod.rs:1786`) only when
    `on_wedge_retired` is configured; the `BatchContext` sender half is constructed
    at the same site and threaded into the batch loop. (b) **join on shutdown** —
    extend `ListenerState` (`mod.rs:1392-1400`, currently `handle`, `scan_handle`,
    `shutdown_tx`) with an `Option<tokio::task::JoinHandle<()>>` for the heal actor;
    `shutdown()` (`mod.rs:2456-2487`) signals it via the same `shutdown_tx` watch
    channel (or drops its request-channel sender to end its receive loop) and awaits
    its handle alongside `handle`/`scan_handle`, so a bus that never started a
    listener never has a heal actor to leak either. (c) **capability narrowing —
    no bus clone**: `PgEventBus` is `#[derive(Clone)]` over `Arc`-backed fields
    (`mod.rs:1403`), so a heal actor holding a full bus clone would keep the bus's
    shared state (pool, registry, hwm) alive independently of the caller's own
    handle — exactly the leak this lifecycle design exists to prevent. Instead,
    extract `unsubscribe`'s body into an internal free fn (e.g. `unsubscribe_core`)
    taking only the narrowed handles it touches — these are two *different* fields,
    both needed (step (b) removes Arcs from the projections Vec, step (c) drops the
    registry entry): the projections Vec `Arc` (`mod.rs:1304,1410`), the
    `subscriber_modes` registry `Arc` (`mod.rs:1429`), the `retired_ids` tombstone
    `Arc`, `hwm` (`mod.rs:1420`), the pool, and a `ReliableDeliveryConfig` clone (it
    derives `Clone`, `config.rs:350`) for the ledger resolve's `bus_name` (=
    `config.events_table`, `mod.rs:327,400`) and the Coordinated-mode check
    (`config.instance_mode`, `mod.rs:4300`). `release_subscriber_lock`
    (`mod.rs:3499`) is today an inherent `&self` method on `PgEventBus` — either
    inline its SQL directly in `unsubscribe_core` or extract it as a second
    pool-only narrowed helper the core delegates to; either way it must not require
    a full bus handle. The inherent `PgEventBus::unsubscribe` delegates to
    `unsubscribe_core`, and the heal actor is constructed with its own clones of
    those same sub-handles — never a `PgEventBus` clone.
  - Create tests in `epoch_pg/tests/pgeventbus_integration_tests.rs` (backoff tested
    with tiny injected intervals, the suite's established pattern):
    `test_wedge_heal_fires_once_per_generation_on_gap_unproven`,
    `test_wedge_heal_never_fires_for_skip_after_backstop`,
    `test_wedge_heal_never_fires_for_deser_wedge`,
    `test_heal_allocates_fresh_generation_ids` (`#gen2`, `#gen10`, near-miss
    rejection), `test_heal_backoff_boot_immediate_rehalt_capped`,
    `test_generation_cap_retires_family_and_errors`,
    `test_healed_replay_delivers_exactly_once` (depends on Phase 1 — the pin that
    would be dishonest without it), `test_unconfigured_bus_never_heals` (default-on
    guard), `test_heal_actor_stops_on_shutdown` (configure a callback, then
    `shutdown()` the bus; assert the heal-actor task terminated — extends the
    existing shutdown-test pattern to the new handle).
  - Explicitly out of bounds: epoch never constructs or `subscribe()`s the fresh model
    (no factory — option (b) settled); no epoch_mem changes (no gap machinery);
    no catacloud-side code; no changes to the fetch exclusions that make the wedge
    inert (`mod.rs:2116-2150`, `2258-2264` — they are what make the deferred heal
    race-free).
- **Entry Conditions**: (1) Phase 1 complete (dedup handoff green; OQ-1 settled
  in-scope, so no external-ticket path); (2) Phase 3 complete (`unsubscribe` +
  tombstone available).
- **Exit Criteria / Verifiable Artifacts**: All new tests pass under
  `EPOCH_REQUIRE_DB=1`; full suite green including the spec 0030 regression pins
  (`Halt` byte-for-byte default; guardrail tests); clippy clean; rustdoc on the new
  config surface states the opt-in contract, the gate, and the cap end-state.
- **Parallelism**: SEQUENTIAL after Phase 1 and Phase 3 (needs both: exactly-once
  heals and the retire primitive). Nothing else may run against the same files
  concurrently.
- **Relative Effort**: M — new config surface, the `BatchContext` channel field, the
  bus-owned heal-actor task with its own lifecycle (spawn/join/shutdown) and a
  narrowed `unsubscribe_core` extraction (the largest plumbing piece), and a full
  wedge-reproduction test battery.
- **Difficulty**: `hard` — races with the inline halt pipeline, time-dependent
  backoff, per-family generation bookkeeping, and a cap end-state that must compose
  correctly with retire semantics.
- **Open Questions / Blockers**: None — all settled 2026-09-16 (OQ-1 in-scope;
  OQ-4/OQ-5 defaults shipped; OQ-9 option (b); an option-(a) future would be a new
  spec, not a rework of this phase). The CLOUD-259 ping is a coordination action at
  phase completion, not a code blocker.

### Phase 5: `EventBus` trait surface, epoch_mem parity, docs & quality gate

- **Goal**: `unsubscribe` is reachable through `epoch_core::EventBus` and implemented
  on `InMemoryEventBus` with the same contract; CHANGELOG and rustdoc cover the whole
  new surface; the repo's quality gates pass.
- **Requirements Covered**: R11, R16
- **Scope**:
  - Modify `epoch_core/src/event_store.rs:231` (`pub trait EventBus`) — add
    `unsubscribe` as a **defaulted** method returning `Ok(false)` (settled non-breaking
    shape, OQ-6; the default body needs no `Self::Error` construction — the trait's
    `type Error: std::error::Error` has no construction bound, so an
    `Err(Unsupported)` default is impossible without a breaking bound). Rustdoc on the
    default states that `Ok(false)` from the default body means "this backend does
    not support removal", distinct from `Ok(false)` from an overriding backend, which
    means "id not found".
  - Modify `epoch_mem/src/event_store.rs` — implement `unsubscribe` on
    `InMemoryEventBus`: removal from `projections: Arc<RwLock<Vec<Box<dyn
    EventObserver<D>>>>>` (`:557`) under the write lock (plain `retain`/`swap_remove`
    by `subscriber_id`); no persisted state to resolve (no checkpoints, in-memory DLQ
    only — paper 1 §7). Rustdoc notes: the mem-bus delivery loop holds the
    projections read lock across the observer loop, including retry sleeps
    (`epoch_mem/src/event_store.rs:618-624`), so unsubscribe here is
    immediate-when-not-mid-delivery rather than pg's documented "next wake" — "same
    contract" (R11) is not "same timing".
  - Modify `epoch_pg/src/event_bus/mod.rs:4165-4170` (`impl EventBus for
    PgEventBus<D>`) — override the trait-default `unsubscribe` by delegating to the
    inherent `unsubscribe` added in Phase 3, so a generic consumer calling
    `EventBus::unsubscribe` on a `PgEventBus` gets the real removal, not the
    trait-default `Ok(false)` no-op.
  - Modify `CHANGELOG.md` (Unreleased) — entries for `unsubscribe` (pg + mem + trait),
    the heal config/callback, the `SubscriberNotFound` flip for retired ids, the
    exactly-once fix, and the CLOUD-259 coordination note (the ping text).
  - Create tests: `InMemoryEventBus::unsubscribe` unit/integration test (subscribe →
    deliver → unsubscribe → publish → assert no delivery; idempotent second call) in
    the epoch_mem test module mirroring the existing subscribe test style; a
    compile-level pin that a minimal third-party `EventBus` impl still compiles
    (defaulted method — existing test doubles may serve).
  - Sweep: `cargo fmt`, `cargo clippy -- -D warnings`, full `cargo test` with
    `EPOCH_REQUIRE_DB=1`.
  - Explicitly out of bounds: no epoch_pg delivery-behaviour changes in this phase
    (the `impl EventBus` override is trait glue delegating to the inherent
    `unsubscribe`, added in the prior bullet); no new dependencies; no P5/heal changes.
- **Entry Conditions**: Phase 3 complete (the contract being mirrored exists); Phase 4
  complete (so the CHANGELOG sweep covers the heal surface in one pass).
- **Exit Criteria / Verifiable Artifacts**: epoch_mem test passes; `cargo build`
  succeeds for a workspace including downstream trait consumers (guardrail: no
  breaking change lands); CHANGELOG diff present; fmt/clippy/test all clean.
- **Parallelism**: SEQUENTIAL after Phase 4 (final gate; also depends on Phase 3's
  contract).
- **Relative Effort**: S — ~10 lines of epoch_mem code plus a trait default and docs.
- **Difficulty**: `standard` — mechanical parity and a documentation sweep.
- **Open Questions / Blockers**: None — settled 2026-09-16 (OQ-6 defaulted `Ok(false)`
  body; the CLOUD-259 coordination note lives in the CHANGELOG; the actual ping is the
  orchestrator's action at spec landing).

### Parallelism Summary

- Phase 1 ∥ Phase 2 — independent (disjoint code regions; shared file only).
- Phase 3 → after Phase 2 (needs the id→Arc registry); may overlap Phase 1 if
  Phase 2 has landed, but the safe default sequence is 1 → 2 → 3.
- Phase 4 → after Phase 1 and Phase 3.
- Phase 5 → after Phase 3 and Phase 4 (final contract/docs/quality gate).

### Effort Summary

| Phase | Effort | Difficulty |
|-------|--------|------------|
| 1 — Exactly-once fresh subscribe over open holes | S | hard |
| 2 — Lock-free id→observer registry | S | standard |
| 3 — `unsubscribe` retire API + removal inventory | M | hard |
| 4 — P5 wedge heal (retire-and-notify) | M | hard |
| 5 — Trait surface, epoch_mem parity, docs gate | S | standard |
| **Total** | **S+S+M+M+S (≈ 3–4 implementation weeks serial; ≈ 2–3 with the Phase 1/2 overlap)** | |

---

## Phases (JSON)

```json
{
  "phases": [
    { "phase": 1, "focus": "Exactly-once fresh subscribe over open holes", "effort": "S", "difficulty": "hard" },
    { "phase": 2, "focus": "Lock-free id-to-observer registry", "effort": "S", "difficulty": "standard" },
    { "phase": 3, "focus": "Unsubscribe retire API with full removal inventory", "effort": "M", "difficulty": "hard" },
    { "phase": 4, "focus": "P5 wedge heal retire-and-notify with fresh generations", "effort": "M", "difficulty": "hard" },
    { "phase": 5, "focus": "EventBus trait surface epoch_mem parity docs gate", "effort": "S", "difficulty": "standard" }
  ]
}
```
