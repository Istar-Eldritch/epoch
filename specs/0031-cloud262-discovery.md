# CLOUD-262 Discovery: observer unsubscribe/retire API and processable ReplayAlways wedges (P5)

## Source

Linear ticket CLOUD-262 (state Triage, priority 2, parent CLOUD-259), fetched via
`linear issue view CLOUD-262` on 2026-09-16, pinned against worktree rev `5c27432`
(`cloud-262` branch = CLOUD-261 merged to main). Full ticket text reproduced below,
followed by empirical verification performed 2026-09-16 by four prober agents
(GLM-5.3-flash via Synthetic) whose reports are listed as verified evidence. The
orchestrator spot-checked the reports' load-bearing code citations against source at
`5c27432`; all verified.

### Ticket text

> ## Problem
>
> Two capability gaps compound in the same area of the event bus:
>
> 1. **No unsubscribe/retire API.** Re-subscribing a projection under a new subscriber id (the vendored-documented remedy for a wedged subscriber: "the remedy is a fresh `subscribe()`", `mod.rs:1888-1893`) leaves the old observer registered forever. Old observers stay in the projections registry, and every listener wake does O(N) observer lock+metadata reads (`mod.rs:1799-1813`, `mod.rs:2086-2094`). A long-lived process that heals N times accumulates N inert observers and slows every batch marginally.
> 2. **ReplayAlways wedges are unprocessable (P5).** P4b's self-heal deliberately excludes ReplayAlways wedges (`mod.rs:1884-1893`), deferring to a "P5 analogue" (`mod.rs:1893`). Until P5 exists, a wedged ReplayAlways subscriber is permanently inert unless the application layer re-subscribes it.
>
> ## Evidence (vendored rev `bed756b`)
>
> * First-registration-wins: duplicate ids get catch-up but no live events (`mod.rs:3353-3377`, `warn_if_subscriber_id_reused`); hence re-subscribes need fresh ids (`#genN` in catacloud's CLOUD-259 Phase 3).
> * Wedged-subscriber exclusion from shared floor / shared batch / private fetch: `subscriber_state.rs:180-188, 223-248`, `mod.rs:2015-2019, 2074`, `mod.rs:1888-1893`.
> * Registry scan cost per wake: `mod.rs:1799-1813`, `mod.rs:2086-2094`.
> * `release_halt` exists in the API but has zero call sites in catacloud; unclear whether it is the intended retirement path.
>
> ## Proposal
>
> 1. `unsubscribe(subscriber_id)` (or retire-by-prefix) that removes the observer from the registry and its checkpoint/floor participation, safe to call for a wedged or idle subscriber.
> 2. P5: define the ReplayAlways wedge remedy inside epoch (auto re-subscribe with a fresh generation id + bounded backoff, mirroring what catacloud's CLOUD-259 Phase 3 now does at the application layer: boot-registration halt → immediate; heal-generation re-halt → 30s/60s cap, `GapUnproven`-only).
>
> ## Coupling warning for CLOUD-259 Phase 3
>
> catacloud's `PolicyGraphHaltResilience` (commit `614c06ae`, branch `cloud-259`) is review-verified safe **under current invariants**: a wedged ReplayAlways observer never processes another event and can never halt again (set-once `halt_fired`, `subscriber_state.rs:375-385`; excluded from every fetch path), and `release_halt` is unused — so its boot-branch spawn cannot race a pending backoff timer. **If P5 makes ReplayAlways wedges processable again, those invariants break** and the boot branch needs a generation-watermark check (skip the immediate spawn when the generation counter advanced past the value captured at scheduling). Whoever ships P5 should ping CLOUD-259.
>
> ## Relationship to CLOUD-259
>
> * (1) bounds the inert-observer accumulation the Phase 3 heal inherently creates (review P2, "unbounded chain cost under a never-clearing fence").
> * (2) would subsume the Phase 3 application-layer heal into the framework, letting catacloud delete `integration/src/policy_projection_heal.rs`.

## Empirical verification (2026-09-16, rev 5c27432, four GLM-5.3-flash probers)

Reports (all under `docs/probes/`, cited by the spec as verified evidence):

1. **`cloud262-retire-semantics-paper-20260916.md`** — retire/unsubscribe semantics paper (pure analysis).
2. **`cloud262-p5-design-space-paper-20260916.md`** — P5 wedge-remedy design-space paper (pure analysis).
3. **`cloud262-wedge-retire-readiness-probe-20260916.md`** — live-Postgres probe (scratch DB `probe_c262_a`).
4. **`cloud262-fresh-subscribe-dedup-probe-20260916.md`** — live-Postgres probe (scratch DB `probe_c262_b`).

### Verified premises (anchors re-checked at 5c27432)

* All five ticket line-anchor claims hold (anchor drift from `bed756b` is expected and documented in the papers).
* P4b still excludes ReplayAlways wedges: `epoch_pg/src/event_bus/mod.rs:2116-2131` — "the remedy is a fresh `subscribe()`, R9b — the `ReplayAlways` floor-exclusion analogue lands in P5".
* No removal path exists: zero `unsubscribe`/`retire` matches across all five crates (CLOUD-231: first-wins, no unsubscribe).
* A wedged ReplayAlways+Halt subscriber is inert (GapUnproven ~5.0 s after the gap becomes visible with `snapshot_fencing: false`; hwm pins; zero post-wedge deliveries; exactly one halt ever) and **pins `wait_until_all_caught_up` forever** — the gate returned false in 3/3 rounds with the wedged id registered and true on a registry without it (report 3, batteries B1/B2).
* A halt is state-based exclusion, not CLOUD-228 head-of-line blocking: a Checkpointed peer delivered 10/10 post-wedge events, median publish→`on_event` 0.64 ms (report 3, B3).
* Registry wake cost measured ≈8.6 µs/subscriber median (12 µs p95) at N=0/1/10/50 parked subscribers (0.652/0.685/0.802/1.082 ms publish→delivery medians) — an upper bound; N ≤ 10 is within noise (report 3, B4).
* First-wins re-subscribe confirmed: old observer keeps live events, same-id new observer is inert for live events but still gets the full catch-up replay (hwm reset to 0, `mod.rs:4343-4345`); `warn_if_subscriber_id_reused` at `mod.rs:3758` (report 4, B3). Guardrails hold: `SkipAfterBackstop`+`Checkpointed` → `InvalidSubscriptionConfig` (`mod.rs:4238-4247`); +ReplayAlways accepted (report 4, B4).

### Findings beyond the ticket (probe/paper verdicts the spec must carry)

1. **The fresh-subscribe double-delivery bug (spec 0030 §4, "ticketed separately") reproduces byte-for-byte at 5c27432 and is NOT ticketed on Linear** (searched 2026-09-16). Fresh ReplayAlways+Halt over a burned hole: deliveries `[(1,1),(3,2),(4,2),(5,1),(6,1)]` — exactly spec 0030's observed counts — and the fresh subscriber re-wedges with its own `GapUnproven` at the same hole (report 4, B1).
2. **Localization (sharpened):** the subscribe-time catch-up delivers each above-hole row exactly once (listener stopped: `[(1,1),(3,1),(4,1)]`); the second copies come from the **first live wake**: the listener seeds `SubscriberState` from the pinned hwm with a born-empty `processed_ahead` (`mod.rs:2063-2130`, dedup at `mod.rs:527-535`), blind to what `subscribe()`'s catch-up (`mod.rs:4439-4448`) already applied. Spec 0030's "processed_ahead guards the live path only" phrasing is confirmed with this sharpening. Proposed fix surface: carry a delivered-above-prefix watermark from `subscribe()` into the one-time live state seed (range watermark, O(1)) — not in `catch_up_from_checkpoint` (it already delivers each row once), not by moving the fetch cursor (report 4, B2). It also fires on **every process boot** of a ReplayAlways subscriber over an open historical burn, and the CLOUD-225 fix comment's "does not double-deliver" claim (`mod.rs:2032-2039`) is hole-blind (paper 2).
3. **The listener state maps (`checkpoint_cache`, `pending_checkpoints`, `subscriber_states`, `last_event_ids`) are listener-task-lifetime, not per-wake** (`mod.rs:1872-1885`) — the orchestrator's brief and the ticket both mischaracterized these. A retire API must prune them across tasks, or lingering states keep feeding the shared floor (`mod.rs:2208`) and P4b. Also: shutdown/reconnect `flush_all_pending_checkpoints` can resurrect a deleted checkpoint row, which motivates **retain** (not delete) checkpoint-row policy (paper 1).
4. **Retire must identify observers without locking them**: `invoke_observer_once` (`retry.rs:43-60`) holds the observer mutex across `on_event`, so Vec-scan identification blocks behind a wedged handler; an id→Arc registry (the reason `subscriber_modes` exists, `mod.rs:1429`) is the prerequisite (paper 1).
5. **Recommended P5 shape (paper 2): option (b) — epoch-managed auto-retire + heal callback** — phased A+B like spec 0030. Epoch cannot construct a consumer's `dyn EventObserver` (registry is `Arc<Mutex<dyn EventObserver<D>>>`, `mod.rs:1304`; epoch's own contract: "Epoch provides the trigger, not the rebuild", `config.rs` RebuildNeededCallback doc), so a factory (option a) or callback (option b) is mandatory for any epoch-internal heal; (b) is single-actor race-free by construction, bounds memory by removing the wedged observer, and makes CLOUD-262's two asks one mechanism. Trigger: `GapUnproven`-only, exactly once per (generation, gap) via set-once `halt_fired`; heal gate is precisely `ReplayAlways` + `FailClosed` + `GapPolicy::Halt` + `GapUnproven` (SkipAfterBackstop never gap-wedges, `subscriber_state.rs:413-437`). Fresh `#genN` ids are mandatory (first-wins). Catacloud: under P5 the Phase 3 boot branch double-heals; two concrete guards close it (stale-generation CAS + retired-check via `SubscriberNotFound`); option (b) lets catacloud delete the 531-line `policy_projection_heal.rs` — ping CLOUD-259.
6. **Advisory-lock release is already broken** (session-scoped `pg_try_advisory_lock` on pooled connections; `release_subscriber_lock` has zero callers; cross-session unlock returns false) — unsubscribe's lock-release story must be defined against that reality, not as a new defect (paper 1).
7. **Ledger rows do not fire rebuild callbacks forever**: one firing, then auto-resolve with `'gap_detection'`; retire should mark its rows `resolved_by='unsubscribe'` to silence zombie callbacks (paper 1).
8. **epoch_mem parity is trivial** (`Box<dyn EventObserver>` in an `RwLock<Vec>`, push-only subscribe) and worth adding via the `EventBus` trait — a breaking-change-vs-defaulted-method call for Ruben (paper 1).

### Positions in tension (for the spec to structure, Ruben to decide)

* **Dedup fix in-scope vs prerequisite:** paper 2 argues the catch-up/live double-delivery fix belongs in scope as Part A (it is on P5's mandatory path — every heal over an open burn double-applies then re-wedges). Report 4 independently argues it is a clean standalone fix (O(1) watermark at the subscribe→listener handoff) and could be a prerequisite ticket. Both agree it MUST be resolved before P5 heal ships.
* **Checkpoint rows on retire:** retain (paper 1 recommendation, because `flush_all_pending_checkpoints` can resurrect deleted rows and orphan rows are read only by `get_checkpoint`/`release_halt`/same-id re-subscribe seed) vs delete (cleaner registry hygiene). Paper 1 recommends retain + documented read-back semantics.

## Open questions for Ruben (carried from the papers; the spec should preserve them as OQs, not resolve them)

1. Dedup fix: in-scope Part A or prerequisite ticket (positions above).
2. `unsubscribe` unknown-id contract: `Ok` (idempotent retire) vs `SubscriberNotFound` (readiness-method parity).
3. Retire-by-prefix: ship in 0031 or defer to catacloud-side loop over explicit ids?
4. Heal backoff/cap defaults: mirror catacloud Phase 3 (immediate, then 30s/60s cap) or config-only?
5. Opt-in vs default-on for the P5 heal.
6. `EventBus` trait unsubscribe (breaking change) vs defaulted method vs epoch_pg-only.
7. Does catacloud want the Phase 3 file deleted in the same release that ships P5 (subsume-and-delete), or after a soak window?
