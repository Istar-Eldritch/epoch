# Probe Report: CLOUD-262 — fresh-subscribe double-delivery over a sequence hole

- Date: 2026-09-16
- Repository: `/root/code/epoch-worktrees/cloud-262` (epoch), branch `cloud-262`, rev `5c27432` ("merge: CLOUD-261 sequence-burn resilience for ReplayAlways subscribers (spec 0030)"), clean tree at probe start.
- Mode: **probe** — temporary integration harness `epoch_pg/tests/probe_c262_b.rs` (deleted after the run); scratch Postgres DB `probe_c262_b` (dropped afterwards). Every bus experiment ran on a per-test isolated events table with its own `global_sequence` sequence, so no burn ever touched a shared table. `catacloud` / `catacloud_template` / `test_*` / `epoch_pg_test*` never touched.
- Infrastructure: Postgres 18.4 container `cloud-259-postgres-1`, host port 5457, run with `DATABASE_URL='postgres://postgres:postgres@localhost:5457/probe_c262_b' EPOCH_REQUIRE_DB=1 cargo test -p epoch_pg --test probe_c262_b -- --nocapture --test-threads=1`.

## Summary of verdicts

| Battery | Question | Verdict |
|---|---|---|
| B1 | Does the fresh-subscribe double-delivery (spec 0030 §4) still reproduce at 5c27432? | **VERIFIED — reproduces exactly** (`[(1,1),(3,2),(4,2),(5,1),(6,1)]` at the fresh observer) |
| B2 | Which pass double-delivers? | **The live pass re-delivers what the subscribe()-time catch-up pass already applied** (B2a/B2b isolation + within-run delta) |
| B3 | Same-id re-subscribe: first-wins semantics + WARN text | **VERIFIED** — old observer keeps receiving; new one inert for live events (but gets the catch-up replay); WARN captured verbatim |
| B4 | Guardrails: SkipAfterBackstop+Checkpointed rejected; +ReplayAlways accepted | **VERIFIED** |

## B4: guardrail sanity (spec 0030 R2) — VERIFIED

`subscribe(SkipAfterBackstop + Checkpointed)` → `Err(PgEventBusError::InvalidSubscriptionConfig)`, and `subscribe(SkipAfterBackstop + ReplayAlways)` → `Ok(())` and registered. Verbatim from the run (`probe_b4_config_guardrails ... ok`):

```text
B4 subscribe(SkipAfterBackstop+ReplayAlways) = Ok("Ok(())")
B4 wait_until_caught_up(accepted id) on empty bus = true
B4 subscribe(SkipAfterBackstop+Checkpointed) = Err(InvalidSubscriptionConfig { subscriber_id: "probe262b:b4bad:034bc278-…", reason: "GapPolicy::SkipAfterBackstop requires SubscriptionMode::ReplayAlways: a Checkpointed subscriber must never advance its persisted checkpoint past an unproven gap, or the checkpoint would lead the contiguous prefix (spec 0027 §3.3)" })
B4 wait_until_caught_up(rejected id) = Err(SubscriberNotFound("probe262b:b4bad:…")) (must be SubscriberNotFound)
```

The rejection happens before any side effect: the rejected id never enters the registry (`wait_until_caught_up` → `SubscriberNotFound`), matching the code comment "sits before any side effect of subscribe()" (`epoch_pg/src/event_bus/mod.rs:4228-4247` at 5c27432).

## B2a: catch-up-only baseline (listener STOPPED) — measured

Pre-committed history `e1=1`, burned hole at 2, `e2=3`, `e3=4`; the bus trigger exists but `start_listener()` is never called, so `subscribe()`'s synchronous catch-up pass is the only thing that can deliver. Verbatim (`probe_b2a_catchup_only_listener_stopped ... ok`):

```text
B2a pre-committed: e1=1, hole=2, e2=3, e3=4 (listener never started)
B2a counts (catch-up-only, listener stopped): [(1, 1), (3, 1), (4, 1)]  (total deliveries 3)
B2a halts during catch-up-only: 0 (catch-up does no gap detection)
```

The catch-up pass delivers every committed row above its cursor — including both rows above the hole — exactly once each, records no gap detection and fires no halt. This is the baseline B2b is compared against.

## B1: reproduction at 5c27432 — VERIFIED (bug still present, same counts as spec 0030 §4)

Scenario (`probe_b1_fresh_subscribe_double_delivery_over_hole ... ok`): `snapshot_fencing: false`, `gap_timeout: 500 ms`, isolated events table; subscriber A (`ReplayAlways` + `FailClosed` + `GapPolicy::Halt`) subscribed, e1 committed and delivered; sequence 2 burned (claimed inside a rolled-back transaction); e2 (seq 3), e3 (seq 4) committed above the hole; A wedged; then a FRESH subscriber id B (`ReplayAlways` + `FailClosed` + `Halt`) subscribed over the hole; e4 (seq 5), e5 (seq 6) published after `subscribe(B)` returned; counts read 4 s later.

```text
B1 e1 committed at seq 1; A caught up
B1 hole burned at seq 2; e2 at 3, e3 at 4
HALT subscriber=probe262b:orig:… held_below=2 reason=GapUnproven
B1 A wedged 1.036121402s after burn: held_below=2 (gap_timeout=500ms)
B1 A position after wedge: 1 (head 4; pinned below hole 2? true)
B1 subscribe(B) returned in 3.055754ms
B1 B counts after subscribe() (catch-up pass only, pre-publish): [(1, 1), (3, 1), (4, 1)]  (total deliveries 3)
HALT subscriber=probe262b:fresh:… held_below=2 reason=GapUnproven
B1 B final counts (catch-up + live): [(1, 1), (3, 2), (4, 2), (5, 1), (6, 1)]  (total deliveries 7)
B1 A final counts (control: wedged, pre-hole events only): [(1, 1), (3, 1), (4, 1)]  (total deliveries 3)
B1 head=6 (max row Some(6)); hole=2; B position after everything: 1
B1 B re-wedged: held_below=2
```

The spec 0030 §4 observation ("double-delivers the sequences just above it, observed counts (3,2),(4,2) at bed756b") reproduces byte-for-byte at 5c27432: the fresh observer received 7 deliveries for 5 events; the two rows immediately above the hole (seqs 3, 4) were delivered twice each. Everything else held too:

- The original subscriber A wedged once (`GapUnproven` 1.04 s after the burn ≈ `gap_timeout` + listener cycles), pinned at position 1 below the hole, and — control — received nothing after the wedge (its wedging is not part of the double-delivery; see B2b).
- The fresh subscriber B received the full catch-up (`1, 3, 4`), then re-deliveries of `3, 4` from the live pass plus first deliveries of `5, 6`, then re-wedged at the same hole with its own `GapUnproven` (`held_below=2`) and froze at position 1.
- Delivery counts were stable after the re-wedge (checked at +4 s): a wedged subscriber is excluded from the shared fetch (`wedged_now` filter, `mod.rs:2260-2267`; `compute_shared_floor` skips wedged states, `subscriber_state.rs:242-263`), so the duplicates do not grow without bound — the damage is exactly one extra copy of each above-hole row known at wedge time.

## B2: which pass double-delivers — the LIVE pass (verified by isolation)

**B2a (listener stopped)** — catch-up is the only possible deliverer: counts `[(1,1),(3,1),(4,1)]`, one delivery per committed row, no halts (section above). The catch-up pass never double-delivers on its own.

**B2b (listener running, NO wedged subscriber)** — same pre-committed history, fresh id over the hole, e4/e5 published after subscribe (verbatim, `probe_b2b_catchup_plus_live_listener_running ... ok`):

```text
B2b pre-committed: e1=1, hole=2, e2=3, e3=4
B2b counts after subscribe() (catch-up only): [(1, 1), (3, 1), (4, 1)]  (total deliveries 3)
B2b final counts (catch-up + live): [(1, 1), (3, 2), (4, 2), (5, 1), (6, 1)]  (total deliveries 7)
B2b fresh subscriber wedged: held_below=2
```

Identical counts to B1 without any wedged peer: the double-delivery needs only the hole, not a wedge.

**Attribution.** In both B1 and B2b the counts were snapshotted twice: immediately after `subscribe()` returned (`[(1,1),(3,1),(4,1)]` — the catch-up pass, already measured alone in B2a) and after the live window (`[(1,1),(3,2),(4,2),(5,1),(6,1)]`). The delta — exactly one extra copy of each above-hole row, plus first deliveries of the post-subscribe rows — arrived strictly after registration, and no other deliverer exists in that window: `catch_up_from_checkpoint` is called only from the boot pass (`mod.rs:1846`) and inside `subscribe()` (`mod.rs:4439`); `subscribe()`'s buffer-drain leg queries only `global_sequence > current_sequence` where `current_sequence` is the catch-up pagination cursor (max seq seen = 4), so it cannot return rows 3–4. The second copies come from the listener's shared-fetch live pass.

**Code localization (anchors at 5c27432, all re-verified):**

1. `catch_up_from_checkpoint` (`epoch_pg/src/event_bus/mod.rs:3892-4170`) delivers **every** committed row above its cursor with no gap awareness and no dedup — by design it must (its job is full replay). Over a hole it delivers the rows above the hole and, via `advance_catchup_prefix`'s CLOUD-227 unbroken-prefix guard (`mod.rs:3817,3827`: `event_global_seq != *contiguous + 1` freezes the prefix at the first hole), leaves the ReplayAlways HWM at the prefix end (1), i.e. **below the rows it just delivered**.
2. The live loop seeds per-subscriber state only once per subscriber id (`mod.rs:2063-2130`): for ReplayAlways it seeds `contiguous_checkpoint` from the HWM (2067-2070: "Seed from the in-memory HWM") and constructs `SubscriberState::new_with_event_id` (`mod.rs:2104-2111`), whose `processed_ahead` starts **empty** (`subscriber_state.rs:170-179`). Nothing records what the subscribe-time catch-up already delivered above the prefix.
3. The shared fetch then reads from the floor (`mod.rs:2206-2211`) — 1 here — so the above-hole rows are re-fetched, and the per-row dedup `state.processed_ahead.contains(&event_seq)` (`mod.rs:533`, populated only by the live path's `record_applied!` at `mod.rs:511`) has no memory of the catch-up deliveries → re-delivery.
4. The safety comment at `mod.rs:2033-2035` ("`subscribe()` registers its observer last … does not double-deliver: … the per-event checkpoint check skips anything at or below the checkpoint") silently assumes the checkpoint/position reflects everything below the head. A hole breaks that premise: the position pins BELOW the hole while catch-up has already delivered rows ABOVE it.

So spec 0030 §4's phrasing "`processed_ahead` guards the live path but not the catch-up pass" is directionally right but imprecise: the catch-up pass delivers each row exactly once; it is the **live pass's dedup state that is blind to what catch-up delivered**. The missing piece is the subscribe()→listener handoff of the delivered-above-prefix range.

## B3: same-id re-subscribe (CLOUD-231 first-wins) — VERIFIED

Scenario: subscriber X (`ReplayAlways`+`FailClosed`) subscribed; e1 delivered; a NEW observer object re-subscribed with the SAME id; e2 published after the re-subscribe; 2 s wait. Verbatim (`probe_b3_same_id_resubscribe_first_wins ... ok`):

```text
[WARN] subscribe(): subscriber id 'probe262b:sameid:d739bf21-4e54-4e92-a869-422255939af4' is already registered on this bus. The existing observer keeps receiving events; this new subscription will not receive any live event (first-registration wins).
B3 warn_if_subscriber_id_reused WARN captured: true (full line on stderr)
B3 OLD observer counts: [(1, 1), (2, 1)]  (total deliveries 2)
B3 NEW observer counts: [(1, 1)]  (total deliveries 1)
B3 NEW observer received 1 event(s) via its catch-up replay
B3 OLD observer received 2 event(s) total
```

- The WARN text of `warn_if_subscriber_id_reused` (`mod.rs:3758-3773`) is captured verbatim above and asserted in-process.
- The OLD observer keeps receiving live events (e2 count 1 at the old store).
- The NEW observer is inert for live delivery (e2 never arrives) — the shared batch deduplicates by subscriber id and keeps only the first projections-vec occurrence (`mod.rs:2308-2320`: "Build task inputs, deduplicating by subscriber_id … only the first wins", `seen_sids.insert` at 2320).
- Nuance worth recording: the inert new observer still received the subscribe()-time catch-up replay (1 event, e1), because `subscribe()` resets the shared HWM to 0 before catch-up (`mod.rs:4331-4344`) and catch-up runs unconditionally into the new observer. "Inert" holds for the live path only.

## Method

- Harness: temporary `epoch_pg/tests/probe_c262_b.rs` (4 `#[tokio::test] #[serial]` tests, one per battery), deleted after the run. Patterns copied from the CLOUD-261 probe recipes: per-test `isolated_events_table` (own `global_sequence` sequence), unique LISTEN channel per bus, `claim-in-open-tx-then-rollback` burn, `snapshot_fencing: false` + `gap_timeout: 500 ms` so holes can never self-heal via `FenceCleared`, delivery counts read from the observer's `InMemoryStateStore` (every delivery is a push, duplicates included) joined back to the table for per-`global_sequence` counts.
- Command: `DATABASE_URL='postgres://postgres:postgres@localhost:5457/probe_c262_b' EPOCH_REQUIRE_DB=1 cargo test -p epoch_pg --test probe_c262_b -- --nocapture --test-threads=1`. All four tests passed (`4 passed; 0 failed`).
- Scratch DB `probe_c262_b` was created (`CREATE DATABASE`) and is dropped at the end of the probe; no shared database or table was touched. One incidental WARN during B3 (`slow statement: ALTER TABLE … ADD COLUMN txid … 2.33 s`) is pool contention with sibling test binaries on the shared server; harmless.
- Attribution relies on measured deltas, not inference alone: catch-up-only (B2a), catch-up-then-live without a wedged peer (B2b), and within-run mid/final snapshots (B1 and B2b) agree on every count.

## Premises confirmed / overturned

| Premise | Verdict |
|---|---|
| Spec 0030 §4: a fresh subscribe over any hole double-delivers the sequences just above it (counts (3,2),(4,2)) | **CONFIRMED at 5c27432** — identical counts at the fresh observer |
| Spec 0030 §4: "`processed_ahead` guards the live path but not the catch-up pass" | **CONFIRMED with a sharpening**: the catch-up pass itself delivers each row exactly once (B2a); the double delivery is the live pass re-delivering catch-up-applied rows because the live `SubscriberState` is seeded with an empty `processed_ahead` at a position pinned below the hole |
| The double-delivery requires a wedged subscriber on the bus | **OVERTURNED as a precondition** — B2b reproduces it with a single subscriber and no wedge (a wedged peer is incidental) |
| CLOUD-231 first-wins: old observer keeps receiving, new same-id observer inert | **CONFIRMED** for live delivery (dedup by sid, `mod.rs:2308-2320`); nuance: the new observer still receives the subscribe()-time catch-up replay |
| Guardrails (spec 0030 R2): SkipAfterBackstop+Checkpointed rejected before side effects; +ReplayAlways accepted | **CONFIRMED** (B4) |

## Implications for spec 0031 (P5 auto-heal and the dedup fix surface)

1. **Every P5 heal over an unfilled hole double-delivers, then re-wedges.** P5's remedy is a fresh generation id + replay from 0. B1 shows what that subscribe() does over an unfilled hole: the fold model is rebuilt from the catch-up replay, then the live pass re-applies the rows just above the hole (state corruption for exactly the fold-style projections P5 targets), and the fresh subscriber immediately re-wedges at the same hole (`GapUnproven`, held_below=hole). A boot-halt heal and a heal re-halt both happen over an unfilled hole by definition, so both triggers hit this. The catacloud `PolicyGraphHaltResilience` mirroring (CLOUD-259 Phase 3, `GapUnproven`-only) would then loop: heal → double-apply → re-halt → 30 s/60 s backoff → heal again — each iteration re-double-delivering the above-hole window.
2. **By contrast, spec 0030's SkipAfterBackstop rebuild path is mostly safe**: a rebuild fires only after a late-materialized row FILLS the skipped sequence, so a fresh subscribe at rebuild time usually sees no hole and no double-delivery (not directly measured — code shape only). The exposure returns whenever a rebuild coincides with some other still-unfilled hole.
3. **Fix surface — the subscribe()→listener handoff.** `subscribe()` knows exactly which sequences it delivered above the contiguous prefix: the catch-up pass delivers every row in `(last_sequence, current_sequence]` (`current_sequence` = pagination cursor = max seq seen) and the buffer-drain leg delivers `(current_sequence, drained_max]`; the prefix/HWM ends at the hole. The live loop's one-time state seed (`mod.rs:2063-2130`) starts from the HWM with empty `processed_ahead`. The guard belongs there: carry a "delivered-above-prefix watermark" from `subscribe()` into the seeded state — e.g. seed `processed_ahead` with the contiguous range `(prefix_end, delivered_watermark]`, or add a single `Option<u64>` range-watermark field to `SubscriberState` checked beside the `processed_ahead.contains` test at `mod.rs:533`. A range watermark is O(1) even when a big backlog sits above the hole (a `HashSet` of the same range is what the current live path would accumulate for a fresh subscriber until its wedge fires ~`gap_timeout` later).
4. **Where the fix must NOT go:** not in `catch_up_from_checkpoint` (it delivers each row once; it has no duplicate of its own), and not by moving the live fetch cursor past the hole (the fetch floor and the pinned position are load-bearing for gap detection and CLOUD-227 ordering; `processed_ahead`-style "applied but not contiguous" is the sanctioned representation, spec 0027 R1). The bug is a missing handoff, not a wrong pass.
5. **Scope coupling:** the same seeding site is what P4b/P5 use to re-seed wedged-subscriber state after a fresh `subscribe()`; whoever fixes the handoff should note the fix is mode-independent in code shape (a Checkpointed subscriber's fresh subscribe over a hole has the identical exposure — its checkpoint is pinned below the hole by the same prefix guard while catch-up delivered past it). Only ReplayAlways was measured here.

## Open questions for Ruben

1. In-scope vs prerequisite: should spec 0031 fix the catch-up→live dedup handoff first (small, well-localized: one watermark field + one seed site), or land P5 and accept one double-apply window per heal over a hole? The probe argues prerequisite: P5's heal loop re-triggers the bug every attempt, and heal triggers fire precisely on unfilled holes.
2. Is the Checkpointed-mode variant of the double-delivery (same code path, different position source) worth a 5-minute confirm leg in the fix's tests? Not measured in this probe.
3. Should `warn_if_subscriber_id_reused`'s "will not receive any live event" understate the effect? The inert new observer DID receive a full catch-up replay (B3) — for P5's "fresh generation id" design that is the intended rebuild, but for a genuine same-id accidental re-subscribe the silent full replay into a new observer object may surprise callers; wording or docs could mention it.
4. The `mod.rs:2033-2035` safety comment ("does not double-deliver") is now known false over holes; should it be corrected in the same fix so the next reader doesn't re-derive the broken premise?

## Notes and residue

- All cited anchors verified at 5c27432 during this session; the two B1 halt lines are quoted from the run (one per subscriber; one `GapUnproven` entry each — no halt spam in any battery).
- Timings: A's wedge 1.04 s after the burn (gap_timeout 500 ms + listener cycles); B's re-wedge ≈0.9 s after subscribe(B) returned; counts stable at +4 s in B1/B2b (B2a waits 1 s with no live path to deliver).
- Cleanup: temporary harness `epoch_pg/tests/probe_c262_b.rs` deleted after the run; scratch DB `probe_c262_b` dropped; `git status` shows only this report file (plus sibling agents' files, untouched). No commits made; `specs/` untouched.
