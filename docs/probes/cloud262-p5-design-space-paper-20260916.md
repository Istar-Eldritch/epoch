# Probe Report: CLOUD-262 P5 — design space for processable ReplayAlways wedges

- Date: 2026-09-16
- Repository: `/root/code/epoch-worktrees/cloud-262`, branch `cloud-262`, HEAD `5c27432` (CLOUD-261 merged to main). Clean tree at paper start; the only artifact this paper produces is this file.
- Mode: **design-space paper — pure analysis.** No code changes, no tests added, no Postgres touched. Every epoch anchor below was read and verified against `5c27432` this session; catacloud anchors were read at commit `614c06ae` (branch `cloud-259`).
- Companion material: `specs/0030-cloud261-sequence-burn-resilience.md`, `docs/probes/cloud261-burn-wedge-probe-20260915.md` (the measured wedge and double-delivery numbers quoted here come from that probe's verbatim output), `docs/probes/cloud261-design-space-paper-20260915.md`.

## Summary of verdicts

| # | Question | Verdict |
|---|----------|---------|
| V1 | What exactly does P5 target? | `ReplayAlways` + `FailClosed` + `GapPolicy::Halt` wedged by `HaltReason::GapUnproven` — and nothing else. Once wedged it is fetched by nothing (shared-batch exclusion mod.rs:2257-2282; P4b exclusion of ReplayAlways mod.rs:2133-2137), which is precisely what makes a deferred/backoff heal race-free. |
| V2 | Can epoch heal a wedge internally with no new application surface? | No. The registry stores `Arc<Mutex<dyn EventObserver<D>>>` (mod.rs:1304) and `subscribe` consumes a caller-constructed `T: EventObserver` (mod.rs:4189); the concrete model type is erased. Epoch cannot construct a consumer's observer, so **any** epoch-internal heal needs a factory (option a) or must delegate (option b). Epoch itself says so: "Epoch provides the *trigger*, not the rebuild" (config.rs:254-257). |
| V3 | Which option should spec 0031 pick? | **(b) epoch-managed auto-retire + heal callback**, phased like spec 0030's A+B. Option (a) is deferred (same factory surface, plus a structural double-healer hazard); option (c) leaves fold-style projections permanently unrecoverable. |
| V4 | Is the fresh-subscribe double-delivery bug real at `5c27432`? | Yes — mechanism fully pinned (§4): the subscribe-time catch-up delivers every scanned row (mod.rs:4069) while the position freezes at the hole (mod.rs:3811-3814), and the first live batch re-delivers everything above the pinned HWM because `processed_ahead` starts empty (subscriber_state.rs:170-186; mod.rs:2063-2106, 527-533). It fires for **any** fresh subscribe over **any** open hole — including every process boot of a ReplayAlways subscriber with a historical burn. |
| V5 | Fix it in CLOUD-262 scope, or gate P5 on a prerequisite ticket? | **In scope, as spec 0031 Part A** (§4.3). Every P5 heal runs over an open hole *by construction of its own trigger*; un-fixed, every heal deterministically corrupts the fresh fold — silent damage worse than the visible wedge it remediates. The fix is one handoff of the delivered-above-prefix set into the live dedup state, and it also fixes today's boot-over-hole corruption. |
| V6 | Trigger semantics | `GapUnproven`-only, firing exactly once per (generation, gap) via set-once `halt_fired` (subscriber_state.rs:78, 433-435; fire site mod.rs:766-774). Not `FenceCleared` (lossless, never wedges), not `Released` (operator action; a documented no-op for ReplayAlways). |
| V7 | Fresh-id policy and retire mechanics | A fresh `#genN` id is mandatory: re-subscribing the same id is first-registration-wins — full catch-up history, then zero live events (mod.rs:3758-3789). Correction to the prober brief: `subscriber_states`/`pending_checkpoints`/`checkpoint_cache`/`last_event_ids` are **not** per-wake locals; they are declared once before the listener select loop (mod.rs:1872-1885) and persist across wakes. A retire API must actively clean them (or gate them), or a lingering state keeps feeding the shared floor (mod.rs:2208) and P4b. |
| V8 | Precedence vs `SkipAfterBackstop` | Verified at the resolver: the `SkipAfterBackstop` arm (subscriber_state.rs:413-427) sets neither `halt_fired` nor `backstop_refused`, so such a subscriber never gap-wedges, never enters `wedged_now`, and keeps live service. The heal must gate on `Halt` + `ReplayAlways` + `GapUnproven`; `SkipAfterBackstop` subscribers do not wedge on burns. |
| V9 | Catacloud coupling | `policy_projection_heal.rs` semantics confirmed (boot halt → immediate heal; `#genN` re-halt → 30 s/60 s capped backoff; `GapUnproven`-only). Under P5 its boot branch double-heals; the needed generation-watermark check is two concrete guards (§8.2): a stale-generation CAS against its own counter, plus a retired-check against the new registry API. Option (b) lets catacloud delete the file outright (ping CLOUD-259). |
| V10 | Readiness interplay | A wedged Halt ReplayAlways subscriber pins `wait_until_all_caught_up` until timeout, forever across retries (mod.rs:2817-2836 polls registry positions; snapshot 2971-2978). Retire removes the id from the registry snapshot and the gate resolves again. Downside to document: readiness calls on a **retired** id flip from "pending" to `SubscriberNotFound` (mod.rs:2898-2907, 2718-2731) — an observable API change. |

---

## 1. The wedge P5 remediates — precise statement

The failure chain (spec 0030 §1, all anchors re-verified at `5c27432`):

1. A burn makes a sequence permanently missing. The live batch observes the gap and arms the snapshot fence; `advance_contiguous_checkpoint` is the pure resolver (subscriber_state.rs:341).
2. If the fence stays unproven past `gap_timeout` (default 5 s, config.rs:532), the `FailClosed` + `Halt` arm refuses the backstop: `observation.halt_fired = true` once, `backstop_refused = Some(next)` (subscriber_state.rs:428-437). The caller fires `on_halt(GapUnproven)` on that first refusal only (mod.rs:766-774).
3. `is_wedged()` is now true (FailClosed ∧ (`held_event` or any `halt_fired`), subscriber_state.rs:199-202).
4. Fetch-path exclusion: the subscriber is dropped from the shared batch via `wedged_now` (mod.rs:2257-2262, filter ~2280-2284) and — because it is `ReplayAlways` — is *also* excluded from P4b's private fetch (mod.rs:2133-2137, "the remedy is a fresh `subscribe()`, R9b … the `ReplayAlways` floor-exclusion analogue lands in P5"). A wedged ReplayAlways subscriber is therefore driven by **nothing**: no `on_event`, no state transition, ever again, for the process lifetime.
5. `release_halt` writes a checkpoint row the subscriber never reads and says so honestly (spec 0030 R8; doc mod.rs:2618-2638, ReplayAlways WARN mod.rs:2646-2655). A fresh `subscribe()` re-wedges at the same hole and double-delivers the rows above it (§4).

So the wedge classes that exist for a ReplayAlways subscriber are exactly two, distinguished by the resolver path that set the state:

- **Gap wedge** (`GapUnproven`, `gap_first_seen[seq].halt_fired = true`) — P5's target.
- **Held-event wedge** (`DeserializeFailure` / `ObserverFailure`, `held_event = Some(seq)`, mod.rs:553-576 and 676-688 live; mod.rs:3995-4021 and 4077-4089 catch-up) — *not* a P5 target: the cause is a bad payload or a broken observer, and an auto-replay would re-hit the same row and re-halt forever (catacloud reaches the same conclusion: "other reasons need operator action", policy_projection_heal.rs:~340-350).

## 2. Option space for the remedy

Notation: **(a)** epoch-internal auto-heal behind an observer-factory hook; **(b)** epoch-managed auto-retire + heal callback (application re-subscribes); **(c)** status quo.

### 2.1 The constraint that shapes everything: who constructs the fresh model

The registry is `type Projections<D> = Arc<Mutex<Vec<Arc<Mutex<dyn EventObserver<D>>>>>>` (mod.rs:1304). `subscribe<T: EventObserver<D>>` takes a caller-constructed value and wraps it (`Arc::new(Mutex::new(projector))`, mod.rs:4204-4206). Past that point epoch holds only the erased trait object: it can clone the `Arc`, lock it, and call its methods, but it cannot name the concrete type, cannot call `clone()` on it (no such trait bound), and cannot build a second instance. Epoch's own rebuild contract states the boundary verbatim (config.rs:252-258):

> "Drop the affected model and re-`subscribe()` it. … Epoch provides the *trigger*, not the rebuild: it cannot reconstruct a consumer's in-memory state for it."

Consequence: options (a) and (b) are the only shapes that heal at all, and both require new application-provided surface — a **factory** (a) or a **callback** (b). "Epoch heals silently with no new API" is not a designable option.

### 2.2 Comparison

| Dimension | (a) epoch-internal auto-heal (factory) | (b) epoch-managed retire + heal callback | (c) status quo |
|---|---|---|---|
| Wedge classes covered | GapUnproven only, `Halt`+`ReplayAlways` only (same gate as (b)) | Same gate; plus the generation **cap end-state** falls out naturally (cap → retire + alert) | SkipAfterBackstop (spec 0030) is the only remedy; Halt wedges stay permanent |
| API surface | New per-subscriber factory hook (e.g. a defaulted trait method or a `subscribe_with_heal` variant) + heal policy config; epoch mints the fresh id and calls the factory per generation | The CLOUD-262 retire/unsubscribe API (ticket ask 1 — prerequisite) + one config callback + heal policy config | None (already shipped) |
| Who constructs the fresh model | The factory, per generation, at epoch's request — application code runs inside the bus's heal path | The application, in its own task, woken by the callback — cleanest ownership; the application also chooses the fresh id | Nobody (the model stays wedged) |
| Old wedged observer | Stays registered and inert (catacloud's current situation: "memory grows by one inert observer per halt", policy_projection_heal.rs:74-77) | **Removed** from the registry and from floor/checkpoint participation — memory bounded, readiness unblocked | Stays wedged forever |
| Double-healer hazard | Real: the application almost certainly also has an `on_halt`-based heal today (catacloud does); two healers racing the same halt → two fresh generations replaying onto shared state (§8.2). Must be disarmed by convention or detection | **None by construction**: epoch's only actions are retire + notify; the application is the single heal actor | N/A |
| Safety of the heal itself | Heal replays from 0 under a fresh id → hits the double-delivery blocker (§4) on every heal over a hole | Same blocker applies to the application's re-subscribe — which is exactly why Part A must precede Part B (V5) | No heal → no new exposure, but no recovery either |
| Catacloud migration | Delete its heal only after proving epoch's ids and catacloud's generation bookkeeping never collide | Delete `policy_projection_heal.rs` (531 lines) and move the logic into the callback — smallest delta, explicitly the outcome CLOUD-259 wants | Keep 531 lines of integration-layer workaround indefinitely |
| Readiness while wedged | Gate pinned until the fresh generation converges (bounded by backoff) | Gate pinned only until retire lands; then the id leaves the registry snapshot (mod.rs:2971-2978) and `wait_until_all_caught_up` resolves (mod.rs:2817-2836) | Gate pinned forever |

### 2.3 Why (b) over (a)

Both need the same application-provided construction surface. (a) spends that surface on a factory so epoch can drive the whole loop itself; (b) spends it on a callback so the application drives its own re-subscribe with epoch handling lifecycle (retire the wedge, keep the registry and floor clean, bound the backoff). Three things tip it:

1. **Single-actor healing.** (b) is race-free by construction: epoch never re-subscribes, so there is exactly one heal actor. (a) always coexists with whatever `on_halt`-driven healing the application already has, and disarming that is a convention, not a guarantee (§8.2 spells out the race for catacloud).
2. **Lifecycle cleanup.** Only (b) removes the wedged observer. (a) accumulates one inert registered observer per heal — unbounded under a halt storm that exceeds the generation cap, unless (a) also gains retire, at which point (a) is (b) plus a factory.
3. **The unsubscribe API is already the ticket's ask #1.** (b) is the option that makes CLOUD-262's two asks (retire API, P5) one coherent mechanism instead of two parallel ones.

(a) remains a reasonable future extension — a factory-based full-auto mode for applications that want zero heal code — and should be recorded as deferred, not rejected, in spec 0031.

### 2.4 (c) precisely stated

Status quo is not "no remedy": `GapPolicy::SkipAfterBackstop` (spec 0030 Part A) exists and is safe for its intended class. The gap is narrow and real: a fold whose state is **not** a pure function of present rows cannot take the audited skip (its rebuild would not be correct), so it must run `Halt`, and under (c) a `GapUnproven` wedge on such a subscriber is permanent: no delivery, no recovery in place, readiness gate pinned, and — per the burn-wedge probe — a fresh `subscribe()` is a defective remedy (re-wedge + double-delivery). Catacloud carries 531 lines of integration-layer machinery standing in for exactly this gap (policy_projection_heal.rs header; spec 0030 §1.2 Part A rationale).

---

## 3. Trigger semantics: `GapUnproven`-only

### 3.1 Why not `FenceCleared`

The fence arm advances past a proven-permanent gap under **both** policies and both failure modes — resolver doc: "The fence-clear branch is untouched: an event proven never to have existed is advanced past under both modes" (subscriber_state.rs:323-325; arm at 404-424). Its only output is a `debug`-log partition with "no data loss" (mod.rs:776-804). No wedge forms, no halt fires, the subscriber stays healthy and in live service. A heal triggered on `FenceCleared` would tear down a working subscriber for a proven-lossless advance.

### 3.2 Why not `Released`

`HaltReason::Released` (config.rs:92-99) fires from `release_halt` (mod.rs:2667-2673) — an explicit operator action. For a `Checkpointed` subscriber the release genuinely resumes delivery (the private fetch re-seeds via `adopt_released_cursor`, which folds `processed_ahead` and clears gap bookkeeping, subscriber_state.rs:205-220). For a `ReplayAlways` subscriber it is a documented no-op on the live subscriber: the row it writes is never read, and the WARN says "delivery does NOT resume … The remedy is a fresh subscribe() with a fresh model" (mod.rs:2618-2638 doc; WARN 2646-2655; probe Q4b verified: `Ok(())`, checkpoint row written, position still pinned).

A heal on `Released` would therefore be wrong in both directions: for Checkpointed subscribers it would re-subscribe an operator's deliberately-resumed subscriber mid-recovery, and for ReplayAlways it would be a redundant second signal behind the `GapUnproven` heal that already fired at wedge formation (set-once `halt_fired` guarantees the wedge-entry halt preceded it).

### 3.3 Interaction with set-once `halt_fired`

`GapObservation.halt_fired` is per-gap, set-once: the resolver sets it on the first backstop refusal and never returns `backstop_refused` for that gap again (subscriber_state.rs:75-78 field doc "fires on halt ENTRY only"; 433-435 set site; 428-432 comment "Subsequent cycles for the same gap return None"). The fire site is the only `HaltReason::GapUnproven` producer in the codebase (mod.rs:766-774).

Two precise consequences for a heal keyed on this callback:

1. **Once per (generation, gap), not once per subscriber.** A subscriber wedged on gap A can refuse a *second* gap B above it and fire a second `GapUnproven` (fresh `GapObservation`, `halt_fired[B] = false` until its own timeout). The heal must therefore be idempotent per generation — keyed on the subscriber family, not the gap — exactly as catacloud does with the `retry_pending` swap and generation allocation (policy_projection_heal.rs:345-355).
2. **The fire site is inside the delivery pipeline.** `fire_on_halt(GapUnproven)` runs in `process_subscriber_for_batch` on the batch that observed the timeout (mod.rs:766-774), and the HaltCallback is awaited inline on that task. A heal must not run its replay inline in the callback — catacloud spawns a task for exactly this reason ("Spawned — the callback is awaited inline by the bus's batch loop and must not run a full replay there", policy_projection_heal.rs:335-336). An epoch-side implementation inherits the same rule.

### 3.4 Interaction with the fetch-path exclusions

After the halt, the wedged ReplayAlways subscriber is excluded from the shared batch (`wedged_now`, mod.rs:2257-2282) *and* from P4b's private fetch (mod.rs:2133-2137). Nothing will deliver to it, and nothing will advance its HWM. This is the property that makes a **deferred** heal safe: there is no racing delivery to the old observer between the halt and the heal — it is inert by construction. It also means the heal trigger cannot be "detect the wedge on the next fetch": the wedge is only ever observed at the moment of formation (the `backstop_refused` fire) or from outside the fetch paths (the callback, or a state poller). The callback is the natural, already-wired hook.

---

## 4. BLOCKER: fresh-subscribe catch-up/live double-delivery over a hole

Spec 0030 §4 (last bullet) records this bug and defers it: "`processed_ahead` guards the live path but not the catch-up pass, so a fresh subscribe over *any* hole double-delivers the sequences just above it (observed counts (3,2), (4,2)) … ticketed separately." The prober brief verifies no such ticket exists on Linear as of 2026-09-16. The measured counts come from the burn-wedge probe's Q4a (verbatim: `[(1, 1), (3, 2), (4, 2), (5, 1), (6, 1)]`, plus a re-wedge at the hole).

### 4.1 Mechanism, pinned at `5c27432`

The two deliveries come from the subscribe-time catch-up pass and the first shared live batch. Step by step, with the handoff that fails to exist:

1. `subscribe()` resets the ReplayAlways HWM to 0 ("a fresh subscribe … rebuilds its in-memory model from empty, so reset the HWM before catch-up", mod.rs:4331-4344) and runs `catch_up_from_checkpoint` (call at mod.rs:4439).
2. The catch-up pass paginates `WHERE global_sequence > $1` (mod.rs:3992-3999) and **delivers every scanned row** through `process_event_with_retry` (mod.rs:4069-4074) — the row loop consults neither `processed_ahead` nor any hole check. `grep processed_ahead` over the catch-up function returns zero hits; the dedup check exists only in the live-batch macro (mod.rs:533).
3. `advance_catchup_prefix` advances the HWM only across an unbroken prefix — `if event_global_seq != *contiguous + 1 { return; }` (mod.rs:3811-3814, the CLOUD-227 guard; doc "freezes for the rest of the pass at the first hole", mod.rs:3791-3800). Over a hole at S: rows S+1.. are **delivered** while the HWM pins at S-1.
4. The pagination cursor advances past the hole regardless (mod.rs:4090-4103, "advances by the maximum sequence seen so the pass terminates at head even past an unfilled hole"), so the pass completes.
5. The listener's init pass seeds the fresh subscriber's live state from the HWM: `contiguous_checkpoint = S-1`, and `processed_ahead` is born **empty** (`SubscriberState::new_with_event_id`, subscriber_state.rs:170-186; seeding site mod.rs:2063-2106).
6. The first live batch fetches from the shared floor (≥ S-1) and, for this subscriber, every above-hole row fails both skip checks — `event_seq <= contiguous_before` is false (mod.rs:527-529) and `state.processed_ahead.contains(&event_seq)` is false on the empty set (mod.rs:533) — so each is **delivered a second time**. The resolver then sees the hole and the fresh generation wedges at S after `gap_timeout` (subscriber_state.rs:428-437 → mod.rs:766-774).

The subscribe-time buffer drain (mod.rs:4477-4501) is cursor-deduped (`WHERE global_sequence > $1(cursor)`, mod.rs:4489-4501) so it never re-delivers catch-up's rows — but rows it *does* deliver above the hole sit in the same exposed window and are re-delivered by live by the same argument. A listener restart adds further copies (the R2 startup pass, mod.rs:1798-1846, re-runs `catch_up_from_checkpoint` from the surviving HWM).

### 4.2 Three sharpened facts beyond the spec's one-line record

- **The bug is not P5-specific — it is a standing correctness bug for the primary ReplayAlways configuration.** The ReplayAlways HWM is in-memory only (mod.rs:1420, "Never persisted: a crash loses it and the next boot replays from 0, which is the intended contract"). Every process boot of a ReplayAlways+Halt subscriber over a stream containing *any* unfilled hole below head re-executes the double-delivery. CLOUD-259 observed burns are endemic under load (spec 0030 §1), so "a historical burn exists" is the common case, not the edge case.
- **The in-code safety claim that excludes this case is hole-blind.** The CLOUD-225 fix comment asserts a mid-drain subscribe "does not double-deliver: … the per-event checkpoint check skips anything at or below the checkpoint" (mod.rs:2032-2039). That argument holds only while the checkpoint equals the maximum delivered — i.e. an unbroken prefix. Over a hole the checkpoint pins *below* the delivered max, and the check passes the already-delivered rows through. The comment should be amended when the bug is fixed.
- **The live path's dedup set is the correct landing place for the fix, and the resolver already knows how to consume it.** `processed_ahead` is consulted by the resolver's first rule — "If the next sequence is in `processed_ahead` → advance (event was processed out-of-order)" (subscriber_state.rs:296, implementation 355-358) — so seeding it at the handoff both stops the re-delivery *and* lets the position cross the hole contiguously once the gap resolves (fence clear, backstop, or skip), without re-delivery.

### 4.3 Fix in scope vs prerequisite ticket — recommendation: **in scope, as spec 0031 Part A**

The fix shape: `catch_up_from_checkpoint` (and the drain leg sharing its counters) must hand the live path the set of sequences it delivered above the pinned prefix — the rows in `(contiguous, cursor]` that actually exist — and the listener's init pass must seed that set into the fresh subscriber's `processed_ahead` (subscriber_state.rs:170-186 is the single seeding constructor; the init pass at mod.rs:2063-2106 is the single production seeding site). Sizing note: the set is the tail above the first hole, normally small for the heal case (holes form near head under load); a documented bound/cap is worth an explicit line in the spec. Seeding is strictly better than the alternative fix shapes: making the catch-up pass stop *delivering* above a hole would strand the backlog of every fail-open/Checkpointed subscriber behind any permanent hole (delivery-above-the-pinned-checkpoint is load-bearing for them, per the CLOUD-227 design), and raising the live seed to the cursor would violate CLOUD-227 and permanently strand late fills below the cursor.

Why in scope rather than a prerequisite ticket:

1. **The blocker sits on P5's mandatory path, not beside it.** A heal's trigger is, by construction, an open hole that refused to clear. Every (a)/(b) heal over that hole executes a fresh subscribe from 0 — the exact reproduction. Un-fixed, P5 automates deterministic fold corruption: the sequences immediately above the hole are applied twice into the fresh model, silently. A visible, inert wedge that auto-heals into silent corruption is a regression on the wedge, and it directly violates the contract P5 exists to honour ("the fold becomes correct again").
2. **It is small, local, and independently valuable.** One handoff set + the two seeding/consumption sites above + tests; it fixes today's boot-over-hole corruption for every ReplayAlways deployment regardless of P5, and hardens spec 0030's own rebuild path (a `RebuildNeededCallback`-driven fresh subscribe is hole-free at the healed sequence, but a *second*, still-open hole below head re-exposes it).
3. **The alternative — gating P5 on a separate ticket — recreates spec 0030 §4's failure mode:** the bug was already deferred once "ticketed separately" and the ticket was never filed. Folding it in as Part A of an A+B spec (the structure 0030 used) is the proven shape, and it lets Part B's TDD pins assert *exactly-once* delivery across a heal — a property that is false today and would be dishonest to pin later.

---

## 5. Fresh-id policy, first-registration-wins, and retire-by-prefix

### 5.1 First-registration-wins makes fresh ids mandatory

`warn_if_subscriber_id_reused` (mod.rs:3758-3789) documents the hazard verbatim: the listener's per-priority dispatch dedups by subscriber id (the `seen_sids` first-wins filter, mod.rs:2318-2327) and state seeding is `contains_key`-gated (mod.rs:2063), so "a second `subscribe()` of an id already registered gets full catch-up history and then never receives a live event: the first observer keeps driving the subscriber silently." This is CLOUD-231's known behaviour (spec 0027 cites it at specs/0027:146), reachable through the *documented* ReplayAlways re-subscribe path, hence a warning rather than an error.

Two aggravating details verified in the same function: the registry `insert` **overwrites** the mode mapping (mod.rs:3782-3784) even though the old observer keeps driving — so after a same-id re-subscribe, `subscriber_mode()` answers with the *new* mode while the *old* observer runs; and `subscribe()` itself zeroes the ReplayAlways HWM for that id before losing the race (mod.rs:4331-4344). Consequence for P5: any heal — epoch-internal or application-side — **must** allocate a fresh id. Re-subscribing the wedged id is not a remedy at any layer.

### 5.2 Fresh-id scheme

Adopt catacloud's scheme as the convention: `{base}#gen{N}`, generation 1 = the boot registration (unsuffixed), each heal allocating the next monotonic value (catacloud `healed_subscriber_id`, imported from the `catacloud_policy` crate, behaviour pinned by `healed_subscriber_id_suffixes_generation` at policy_projection_heal.rs:436, with tests pinning `projection:policy-graph#gen2`/`#gen10` and the gen-1 rejection). Family matching is exact-base OR `base#gen<digits>`, with explicit near-miss rejections (policy_projection_heal.rs:136-155 and its tests) — worth importing into epoch so a heal for `policy-graph-projection` can never match `policy-graph`.

The generation counter is per-process, in-memory. Across a restart the counter resets but the ids in the registry are gone too (registration is in-memory), so collisions require a same-process wrap — impossible with a monotonic `AtomicU64`. No persisted state is needed.

### 5.3 Retire: what must be cleaned, and the correction to the brief

The prober brief describes the listener's per-wake locals as "rebuilt each wake, removal between wakes is clean: `sid_to_proj`, `replay_always_by_sid`, `subscriber_states`, `pending_checkpoints`, `checkpoint_cache`, `last_event_ids`". That is **half right**, and the half that is wrong matters for the retire API:

- Genuinely per-wake (rebuilt each cycle): `sid_to_proj`, `replay_always_by_sid` (mod.rs:2049-2050), `wedged_sids`/`wedged_now` (mod.rs:2133-2137, 2257-2262). A retired observer simply disappears from these next wake — clean.
- **Long-lived, declared once before the listener select loop and persisting across wakes:** `checkpoint_cache` (mod.rs:1872), `pending_checkpoints` (1876), `subscriber_states` (1880), `last_event_ids` (1885). State seeding is additive-only (`if !subscriber_states.contains_key`, mod.rs:2063); nothing prunes. If retire removes an observer from the registry but not from these maps:
  - a lingering `subscriber_states` entry keeps feeding `compute_shared_floor` (mod.rs:2208, called over `subscriber_states.values()`) — the retired subscriber still pins the shared fetch floor (harmless for a wedged ReplayAlways retiree only because `is_wedged` excludes it from the floor — but a retire of an *idle* subscriber, ticket ask 1, leaves a non-wedged state that pins the floor forever at its last position);
  - P4b's wedged scan (mod.rs:2133-2137) iterates `subscriber_states` and would keep privately fetching a retired Checkpointed wedge against its persisted checkpoint row.

So the retire design (CLOUD-262 ask 1) needs a cross-task removal mechanism, not just a registry `Vec::retain`: the natural shape is a shared retired-id set (or tombstone map) that the listener consults each wake to prune the four long-lived maps and to gate floor participation — mirroring the existing snapshot-then-release pattern (mod.rs:2036-2041). Residue from the *current* wake's snapshot is bounded and benign: the Arc-cloned observer may still be driven once (the CLOUD-225 comment's "picked up on the next wake" reasoning, mod.rs:2036-2041), and at-least-once semantics already tolerate that.

### 5.4 Retire-by-prefix and what retire must touch

`unsubscribe(subscriber_id)` (exact) plus `retire_by_prefix(base)` (the whole `base`/`base#genN` family) — the prefix form is what retires a generation chain in one call (after K heals there are K+1 registered observers; the cap end-state and process shutdown both want the chain gone atomically). Full inventory of what retire must touch:

1. The registry Vec (removal under the outer mutex — short critical section by design, mod.rs:1290-1303).
2. `subscriber_modes` (readiness registry): its removal is what unblocks `wait_until_all_caught_up` (snapshot at mod.rs:2971-2978; poll loop 2817-2836) — and what makes per-id readiness calls fail fast with `SubscriberNotFound` (mod.rs:2898-2907, 2718-2731, 2804-2810) instead of polling a dead subscriber forever. That error flip is an observable API change to document in the spec.
3. The HWM map entry for ReplayAlways (mod.rs:1420) — otherwise `subscriber_position`/lag for a re-registered id could observe a stale generation's mark.
4. The listener's long-lived maps via the retired-set mechanism (§5.3).
5. Checkpoint rows: for a Checkpointed retiree, decide delete-vs-keep. Keep is safer (audit; `release_halt` already deliberately writes rows for unregistered ids — "operator releasing ahead of registration", mod.rs:2639-2641 — so rows for absent subscribers are an accepted state). A later re-subscribe of the same id resumes from the row; for ReplayAlways there is no row to consider. `epoch_event_bus_gap_timeouts` rows keyed by the retired id should stay (they are audit records; `check_skipped_gaps` fires callbacks per row — a spec line should note that a retired id's unresolved rows still surface in scans).
6. Coordinated mode: the advisory lock is keyed by subscriber id (mod.rs:3477; acquired in subscribe). Retire should release it (`release_subscriber_lock` exists, mod.rs:~3495-3511) so a new instance can acquire the id immediately.

`epoch_mem` is unaffected: its observers live in `Arc<RwLock<Vec<Box<dyn EventObserver<D>>>>>` with a push-only subscribe (epoch_mem/src/event_store.rs:~558, 618-624, 755-769), it allocates no sequences and has no gap machinery, so there are no wedges to heal and no retire to mirror. P5 is `epoch_pg`-only; the spec should say so explicitly.

---

## 6. Precedence vs `SkipAfterBackstop` (spec 0030)

Verified from the resolver arms (subscriber_state.rs:413-437):

- **`FailClosed` + `SkipAfterBackstop` arm (413-427):** returns the skip as `AdvanceOutcome::pending_backstop_skip` (field at 288) *without mutating state* — the comment is explicit: "no halt_fired, gap retained". The caller applies it only after confirming the ledger row (mod.rs:727-762); on a write failure the subscriber sits in the refused-backstop posture and the skip is re-offered next tick. Since `is_wedged` is `FailClosed ∧ (held_event ∨ any halt_fired)` (subscriber_state.rs:199-202) and neither is ever set on the gap path for this policy, the subscriber **never gap-wedges**: it is never in `wedged_now` (mod.rs:2257-2262), keeps being served by the shared batch, and burns do not stop it. (It *can* still wedge on `held_event` — deser/observer halts set it (mod.rs:553-576, 676-688) — but those are not gap wedges and not P5's trigger.)
- **`FailClosed` + `Halt` arm (428-437):** the only path that sets `halt_fired` on a gap and produces `backstop_refused` → `on_halt(GapUnproven)` (mod.rs:766-774) → the wedge.
- The registration guardrail confines `SkipAfterBackstop` to ReplayAlways subscribers (`SkipAfterBackstop` + `Checkpointed` → `InvalidSubscriptionConfig`, mod.rs:4228-4247), so the class P5 heals is exactly **`ReplayAlways` + `FailClosed` + `GapPolicy::Halt` + `HaltReason::GapUnproven`** — the conjunctive gate the heal must implement. `gap_policy` is resolved once per state init (subscriber_state.rs:120-133) and re-resolved from the observer each wake (mod.rs:2054-2058), so the gate is available wherever the heal hooks.

So the precedence statement for spec 0031: **the heal applies only to wedges that `SkipAfterBackstop` subscribers cannot form.** A `SkipAfterBackstop` subscriber's recovery is already spec 0030's audited skip + detection + rebuild; P5 must never fire for one, and the gate above guarantees it structurally (no `GapUnproven` is ever produced for that policy).

---

## 7. Config surface and observability

### 7.1 Opt-in by construction, not by convention

The remedy cannot be default-on, and not merely as policy: without an application-provided factory (a) or heal callback (b) epoch has nothing to call — the fresh model is unobtainable (§2.1). Providing the hook *is* the opt-in. That is the right shape: it makes "epoch healed my subscriber and I never asked" impossible, keeps `Halt` byte-for-byte the default behaviour (spec 0030 R6), and means a deployment that ships the hook has explicitly accepted auto-replay of its fold.

Suggested surface on `ReliableDeliveryConfig` (fields and defaults verified at config.rs:351-540: `gap_timeout` 5 s at 532, `snapshot_fencing` true at 537, `on_rebuild_needed` default `None` at ~500, `gap_scan_interval` default `None` at 519):

```text
on_wedge_retired: Option<Arc<dyn WedgeRetiredCallback>>   // option (b): epoch retires, app re-subscribes
wedge_heal: Option<WedgeHealPolicy>                        // shared policy for either option
  pub max_generations: u32        // cap; cap reached → retire chain + alert, no further heals
  pub backoff: BackoffSchedule    // default: immediate (first wedge of a chain), 30 s, 60 s cap
                                  // — the numbers catacloud converged on
```

The backoff distinguishes the two wedge classes catacloud identified and P5 inherits: a **boot-generation wedge** means the fold is stale (events above the hole were never applied) → heal immediately; a **heal-generation re-halt** proves the catch-up already rebuilt the fold (GapUnproven fires only from the live loop) so an immediate re-subscribe adds nothing and loops → bounded backoff (catacloud rationale, policy_projection_heal.rs header §"Implemented semantics", constants at 98-105). Epoch can classify the same way it classifies the family: generation 0 (unsuffixed id) vs `#genN`.

### 7.2 Observability

- **`HaltReason` is `#[non_exhaustive]`** (config.rs:59-60) — adding a variant (e.g. `HealRetired` / `WedgeSuperseded`, carrying the new generation id) is source-compatible for downstream matches that already have a wildcard arm; document it in the CHANGELOG like `InvalidSubscriptionConfig` was.
- **WARN per heal, ERROR on cap.** Catacloud's cadence is the model: one loud line per heal with old id, new id, generation, and held-below sequence (policy_projection_heal.rs:175-183); alert spam under a sustained wedge is naturally bounded by the backoff (module doc, last §Concurrency bullet). At the generation cap: retire the chain + ERROR + final halt-style callback — the operator now owns it.
- **Readiness interplay (all verified):** while wedged, the subscriber's position (ReplayAlways → HWM, mod.rs:2772-2800) pins below head, so `wait_until_all_caught_up` polls it until timeout, every time (mod.rs:2817-2836; registry snapshot 2971-2978) — the gate is honest but stuck. After retire the id leaves the snapshot and the gate resolves. Between halt and heal the gate is stuck for at most the backoff; after a heal, the fresh generation's HWM is also legitimately below the hole until the fence clears, so the gate stays honestly unresolved until convergence. Note the empty-registry hazard next door (mod.rs:2958-2965): a cap-retire that empties the registry makes `wait_until_all_caught_up` return `Ok(true)` trivially — the spec must warn that retiring the last subscriber silently satisfies a readiness gate.
- **Retired-id API surface:** post-retire, `subscriber_mode`/`subscriber_position`/`subscriber_lag`/`wait_until_caught_up` on the old id return `SubscriberNotFound` (mod.rs:2718-2731, 2772-2810, 2898-2907). Applications currently polling a wedged id's lag (as catacloud's heal does after subscribing, policy_projection_heal.rs:204) must handle the error for retired ids.
- **Distinguish the wedge classes in logs:** `GapUnproven` (heals) vs `DeserializeFailure`/`ObserverFailure` (never heal — payload/observer bugs). Catacloud's log ladder already does this ("GapUnproven heals automatically; other reasons need operator action").

---

## 8. Catacloud coupling (CLOUD-259)

File read in full: `/root/code/catacloud-worktrees/cloud-259/integration/src/policy_projection_heal.rs` (531 lines, commit `614c06ae`).

### 8.1 Semantics map (verified against the file)

| Mechanism | Where | Semantics |
|---|---|---|
| Family match | `is_policy_graph_subscriber` (136-145) | boot id exact, or `base#genN` prefix match; near-misses rejected (test `policy_graph_subscriber_family_matches_boot_and_heal_ids`) |
| Generation class | `is_heal_generation` (147-155) | any `#genN` id; a `GapUnproven` on one proves its catch-up completed (fires only from the live loop) |
| Heal action | `HealState::heal` (155-240) | allocate `#genN+1` → up to 3 subscribe attempts 1 s apart → each attempt **clears the shared graph, then subscribes** the fresh generation (clear at 185-190, subscribe at 194); old observer stays registered but inert |
| Boot halt | `on_halt` else-branch (367-377) | `GapUnproven` on the plain boot id → immediate heal + reset `generation_rehalt_count` |
| Heal re-halt | `on_halt` if-branch (339-366) | `GapUnproven` on `#genN` → first re-halt waits 30 s, later ones 60 s (constants 98-105); `retry_pending` swap prevents stacked retries |
| Non-heal reasons | log ladder (287-337) | `Released` → WARN only; deser/observer failures → ERROR, operator action; Telegram inner callback always invoked |
| Failure end-state | `heal()` error path (229-238) | 3 failed attempts → graph left **empty and frozen** (worse than wedged-with-data), operator action required |

The file's header cites vendored-epoch anchors that match what this paper verified at `5c27432` (the CLOUD-227 unbroken-prefix guard = `advance_catchup_prefix`, mod.rs:3811-3814 here; the P4b ReplayAlways exclusion = mod.rs:2133-2137 here).

### 8.2 The ticket's warning, made concrete

The file is safe today because of three invariants: (i) a wedged observer never processes another event (fetch exclusions), (ii) `halt_fired` is set-once so each generation halts at most once per gap, (iii) catacloud's callback is the **only** healer. P5 breaks (iii) — and the boot branch is where it bites first:

**The race.** P5 ships; catacloud has not migrated. Boot subscriber wedges → `on_halt(GapUnproven)` fires (epoch fires it before any heal, at mod.rs:766-774):

1. Catacloud's boot branch classifies: plain id ⇒ "graph is stale" ⇒ immediate heal ⇒ allocates `#gen2`, clears the shared graph, subscribes.
2. Epoch's P5 heal reacts to the *same* halt ⇒ allocates its own fresh id (if epoch mirrors the `#genN` scheme, potentially the *same string* `…#gen2`, since the counters are independent) ⇒ re-subscribes.
3. Now two live generations mount the **same** shared `Arc<RwLock<PolicyGraph>>`. Each heal clears the graph before its own replay (catacloud does, and an epoch-side factory heal would have to as well) — so each clear can land mid-replay of the other, and every event is applied twice. Fold corruption, not just redundancy.
4. Both generations re-wedge at the hole ⇒ two more heals ⇒ generation inflation, plus a fight over `retry_pending` and `generation_rehalt_count` (each boot-branch firing resets the backoff chain the other is sleeping on).

**The generation-watermark check (two guards, both concrete):**

- **Guard 1 — stale-generation CAS (within catacloud).** Before the boot branch heals: atomically verify the halted id is still the newest generation the family knows and claim the next generation in the same step — `compare_exchange` on the watermark (`current == halted_generation` → set `current = halted_generation + 1`), heal only on success. A halt for a superseded generation loses the CAS, logs, and delegates to the alert only. This closes the within-application window regardless of who else heals.
- **Guard 2 — retired-check against the bus (across the P5 boundary).** If epoch's P5 retired the halted subscriber before catacloud's callback runs, the halt is stale news — epoch superseded it. That state is observable with the CLOUD-262 API alone: `bus.subscriber_mode(&info.subscriber_id)` returns `Err(SubscriberNotFound)` for a retired id (mod.rs:2718-2731). The boot branch checks it first and skips the heal on `Err`. (Conversely, if catacloud migrates to option (b) wholesale, both guards and the whole file are deleted — see §8.3.)

Transitional rule for the interim: while catacloud still runs its own heal, epoch's P5 must be **off** for the policy-graph family — which option (b) gives for free, since (b) heals *only* through the application-provided callback; a subscriber with no callback registered is retired-but-unhealed only if a retire is somehow triggered externally. The mutual-exclusion is structural in (b) and conventional in (a) — one more reason for V3.

### 8.3 What P5 makes possible for catacloud

Option (b) is a strict upgrade path: catacloud implements `WedgeRetiredCallback` with the body of today's `heal()` (clear graph → fresh `#genN` projection → `subscribe`), deletes the classification/backoff/watermark machinery from its `on_halt`, and — if epoch also takes over the boot/re-halt backoff policy via `wedge_heal` config — deletes `policy_projection_heal.rs` (531 lines) entirely. The generation counter can move into epoch. That is the outcome the epic wants: "working integration-layer machinery standing in for a framework gap" (spec 0030 §1.2) replaced by the framework. **Ping CLOUD-259 when spec 0031 lands** — the ticket's coupling warning says whoever ships P5 pings them, and §8.2's guards are only needed in the window where both sides can heal.

---

## 9. Recommendation for spec 0031

**Phased, mirroring spec 0030's A+B structure — one spec, two parts, Part A a hard prerequisite for Part B:**

- **Part A — exactly-once fresh-subscribe over open holes (the blocker fix).** Hand the catch-up/drain delivered-above-prefix set into the fresh subscriber's `processed_ahead` (§4.3); seed site `SubscriberState::new_with_event_id` + init pass; TDD anchor: a fresh subscribe over a burned hole delivers every row exactly once (today's counts `(3,2),(4,2)` become `(3,1),(4,1)`), and the position still pins at the hole. Amends the hole-blind CLOUD-225 comment (mod.rs:2032-2039). Standalone value: fixes boot-over-hole double-delivery for every ReplayAlways deployment today.
- **Part B — the retire API + option (b) remedy.** 1) `unsubscribe(subscriber_id)` + `retire_by_prefix(base)` with the §5.4 cleaning inventory (registry, `subscriber_modes`, HWM, listener tombstones for the long-lived maps, advisory-lock release, documented `SubscriberNotFound` flip and checkpoint-row policy). 2) `on_wedge_retired` callback + `wedge_heal` policy (GapUnproven-only, `Halt`+`ReplayAlways`-only gate, immediate-boot / 30 s / 60 s-cap backoff, `max_generations` cap → retire + ERROR), fresh ids `{base}#gen{N}`. 3) Observability per §7.2. 4) The catacloud migration note + CLOUD-259 ping (§8.3).

Option (a) (epoch-internal factory heal) is recorded as **deferred**: it needs the same application surface, adds the double-healer hazard (§8.2), accumulates inert observers unless it also gains retire, and buys nothing (b) does not. Status quo (c) remains available and untouched for everyone who ships neither hook; `SkipAfterBackstop` stays the remedy for pure-function folds, byte-for-byte.

---

## 10. Open questions for Ruben

1. **Blocker placement.** Fix the fresh-subscribe double-delivery inside spec 0031 as Part A (recommended, §4.3), or as a separate ticket that must land first? (Spec 0030 §4 already deferred it once as "ticketed separately" and no ticket exists — a second deferral risks the same outcome.)
2. **Heal actor.** Confirm option (b) (retire + callback) over (a) (factory heal). If (a) is wanted later, should the factory be a defaulted `EventObserver` trait method (per-subscriber) or a bus-config hook (per-bus, one policy for many subscribers)?
3. **Id convention.** Is `{base}#gen{N}` acceptable as an epoch-side convention (catacloud-compatible), and should `retire_by_prefix` be the primary public API with exact-id `unsubscribe` as the primitive, or the other way round?
4. **Retire mechanics.** Retire via a shared tombstone set consulted per wake (recommended, §5.3) vs. some channel-based signal to the listener? Also: should retire of a *Checkpointed* subscriber delete or keep its checkpoint row (recommend keep, §5.4)?
5. **Defaults.** Adopt catacloud's immediate/30 s/60 s backoff as the default schedule, and what `max_generations` cap (catacloud has none — unbounded chain until convergence; a cap forces an explicit retire-and-alert end state)?
6. **Readiness during heal.** Acceptable that `wait_until_all_caught_up` stays unresolved until the fresh generation converges (honest but stuck), with the cap-retire + empty-registry `Ok(true)` hazard (§7.2) documented — or should cap-retire surface a distinct error/callback a gate can honour?
7. **Transitional coupling.** Require catacloud's two watermark guards (§8.2) for any window where P5 and `PolicyGraphHaltResilience` coexist — or mandate the migration in the same release and skip the guards? Who owns the CLOUD-259 ping?
8. **Scope of "wedge".** Keep the heal `GapUnproven`-only permanently (deser/observer wedges always need application action), or leave a door open for a future observer-failure trigger under the same machinery?

---

## Appendix A — Premises confirmed / overturned

**Confirmed (every one re-read at `5c27432`):**

- No `unsubscribe`/removal path exists anywhere (grep across all five crates: zero matches; registry is push-only, mod.rs:4727-4729).
- P4b excludes ReplayAlways wedges (mod.rs:2133-2137) and the shared batch excludes all wedged subscribers (mod.rs:2257-2282) — a wedged ReplayAlways subscriber is driven by nothing.
- `halt_fired` set-once (subscriber_state.rs:75-78, 433-435); `GapUnproven` fires once per gap from the live path only (mod.rs:766-774).
- First-registration-wins on reused ids, with the registry-mapping overwrite trap (mod.rs:3758-3789).
- The subscribe guardrail confines `SkipAfterBackstop` to ReplayAlways (mod.rs:4228-4247); the resolver arms confirm `SkipAfterBackstop` never gap-wedges (subscriber_state.rs:413-437).
- `release_halt` is honest for ReplayAlways post-0030 (mod.rs:2618-2655) and writes rows for unregistered ids deliberately (2639-2641).
- Readiness validates via the registry (`SubscriberNotFound`, mod.rs:1231, 2718-2731; snapshot 2971-2978; poll 2817-2836; empty-registry hazard 2958-2965).
- Catacloud's heal semantics match the ticket's description exactly (§8.1).
- CLOUD-261 anchors from the brief: `Projections` type 1304, `hwm` 1420, `subscriber_modes` 1429, P4b comment 2116-2131, `check_skipped_gaps` 3424, `warn_if_subscriber_id_reused` 3758, `subscribe` 4189, config defaults — all exact; `resolve_gap_timeout` 3446 (brief said ~3455), `try_acquire_subscriber_lock` 3477 (brief ~3494), `fast_forward_all_subscribers` 2999 with snapshot at 3022 (brief ~3012) — minor drift only, no contradictions.

**Overturned / refined:**

- **The brief's "per-wake locals" claim is wrong for four of the seven maps.** `checkpoint_cache`, `pending_checkpoints`, `subscriber_states`, `last_event_ids` are declared once before the listener select loop (mod.rs:1872-1885) and persist across wakes; only `sid_to_proj`/`replay_always_by_sid` (2049-2050) and the wedge sets (2133-2137, 2257-2262) are rebuilt per wake. Design consequence: retire needs an active cross-task cleaning mechanism (§5.3) — removal is *not* automatically clean between wakes.
- **The CLOUD-225 "does not double-deliver" comment (mod.rs:2032-2039) is hole-blind.** Its checkpoint-check argument assumes the checkpoint equals the max delivered; over a hole the checkpoint pins below it and the already-delivered rows pass the check. Amended when Part A lands.
- **The blocker's blast radius is wider than spec 0030 §4 states.** It is not only "a fresh subscribe over any hole" in the heal/rebuild context: every *process boot* of a ReplayAlways+Halt subscriber over a stream with an unfilled historical burn re-executes it (HWM is in-memory, mod.rs:1420). Part A therefore has standalone value independent of P5.

## Appendix B — Method

- Read in full or in targeted ranges, at `5c27432`: `epoch_pg/src/event_bus/mod.rs` (delivery macro and halt fire sites 460-805; R2 pass 1760-1870; listener event-processing block 2000-2390; readiness 2560-3060; gap scan/locks 3400-3520; catch-up 3740-4100; `subscribe` 4200-4740; type/error/field declarations 1180-1465), `epoch_pg/src/event_bus/subscriber_state.rs` (60-465 plus resolver tests via grep), `epoch_pg/src/event_bus/config.rs` (55-340, 480-540), `epoch_mem/src/event_store.rs` (550-775), and `/root/code/catacloud-worktrees/cloud-259/integration/src/policy_projection_heal.rs` (all 531 lines).
- Greps used for exhaustive negatives: `unsubscribe|retire` (zero matches in epoch crates), `processed_ahead` (live-path-only hit map), `nextval`-style production-site inventories from the CLOUD-261 papers reused where cited.
- Measured numbers quoted ((3,2),(4,2) double-delivery; 5.05 s GapUnproven latency; fence-cleared ~1.1 s self-heal) are from `docs/probes/cloud261-burn-wedge-probe-20260915.md`, whose verbatim test output was produced against `bed756b`; the code paths they exercise were re-read line-by-line at `5c27432` and are unchanged in the relevant regions.
- No database, no builds, no tests, no source edits: `git status` shows only this report file.
