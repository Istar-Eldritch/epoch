# Probe Report: CLOUD-262 — wedged-ReplayAlways readiness pin, wedge isolation, and registry scan cost (retire/unsubscribe readiness)

- Date: 2026-09-16
- Repository: `/root/code/epoch-worktrees/cloud-262` (epoch), branch `cloud-262`, rev `5c27432` ("merge: CLOUD-261 sequence-burn resilience for ReplayAlways subscribers (spec 0030)"), clean tree at probe start.
- Mode: **probe** — temporary harness `epoch_pg/tests/probe_c262_a.rs` (4 `#[tokio::test] #[serial]` batteries), to be deleted after the run; scratch Postgres DB `probe_c262_a` on the shared server (host port 5457, container `cloud-259-postgres-1`, PostgreSQL 18.4); all bus experiments on per-test `isolated_events_table`s (own `global_sequence` sequence), so no burn ever touched a shared table.
- Environment: `DATABASE_URL=postgres://postgres:postgres@localhost:5457/probe_c262_a EPOCH_REQUIRE_DB=1 cargo test -p epoch_pg --test probe_c262_a -- --nocapture --test-threads=1`.
- Harness recipes reused from `docs/probes/cloud261-burn-wedge-probe-20260915.md` (fencing-off burn-wedge chain) and `docs/probes/cloud261-alloc-cost-probe-20260915.md` (rig etiquette).

## Summary of verdicts

| Battery | Question | Verdict |
|---|---|---|
| B1 | Does the CLOUD-261 recipe reproduce a wedged ReplayAlways+Halt subscriber at 5c27432, and is it inert with a pinned in-memory hwm? | **Verified** — `GapUnproven` fired **5.015 s** after the gap became visible (gap_timeout = 5 s), hwm pinned at the pre-hole seq 1 while head advanced to 6, **zero** post-wedge deliveries, exactly **1** halt ever. |
| B2 | Does `wait_until_all_caught_up` become unresolvable while the wedged subscriber stays registered, and does a healthy registry on the same tables resolve it? | **Verified** — 3/3 rounds returned `Ok(false)` despite fresh events; fresh bus on the same tables with only healthy subscribers resolved the gate both with a planted-at-head Checkpointed peer and with a fresh ReplayAlways+FailOpen subscriber that consumed the full holey backlog. The contrast is exactly what an unsubscribe API would restore (inference — no unsubscribe API exists to call). |
| B3 | Does the wedged ReplayAlways subscriber block Checkpointed peers' shared-batch progress? | **Verified** — no. Peer delivered **10/10** post-wedge events with median publish→`on_event` latency **0.64 ms** (p95 0.99 ms) while the wedged subscriber stayed pinned at its pre-hole position (lag 13 at head 14). A halt is registry-state exclusion (mod.rs:2258-2264, 2116-2131), not a CLOUD-228 in-flight `on_event` holding the observer mutex. |
| B4 | What does the O(N) per-wake observer scan cost in delivery latency? | **Quantified** — publish→`on_event` latency grows slowly and monotonically with N: medians 0.652 / 0.685 / 0.802 / 1.082 ms and p95s 0.801 / 0.890 / 0.906 / 1.400 ms at N = 0 / 1 / 10 / 50 (3 interleaved rounds each). Net cost ≈ **8.6 µs per parked subscriber (median), 12 µs (p95)** — real but small; N ≤ 10 is within run noise. |

(Numbers finalized in Findings as rounds complete; this report is written incrementally.)

## Method

Harness (all in the temporary `epoch_pg/tests/probe_c262_a.rs`, deleted after the run):

- Per-test `isolated_events_table(pool)` (copy of the suite helper, `epoch_pg/tests/pgeventbus_integration_tests.rs:5747`): `CREATE TABLE ... (LIKE epoch_events INCLUDING ALL)` + own `nextval` sequence, so burns are deterministic and private.
- Bus: `PgEventBus::with_config` + `setup_trigger` + `start_listener`, fresh channel per bus.
- Burn recipe (from the CLOUD-261 probe): `BEGIN; INSERT ... RETURNING global_sequence` on a throwaway stream (claims the next seq), commit 2 events above it on the probe stream, `ROLLBACK` — the claimed seq is permanently burned.
- Wedged subscriber: `ReplayAlways` + `FailClosed` + default `GapPolicy::Halt`, with `snapshot_fencing: false` so the fence cannot prove the burn (the CLOUD-261 recipe's wedge precondition).
- Halt capture: `on_halt` callback recording `(test-relative Instant, HaltInfo)`.
- Position of a ReplayAlways subscriber = its in-memory hwm, observed via `subscriber_lag` (`head − position`, mod.rs:2804) against `SELECT max(global_sequence)`.

## Findings

### B1 — wedge reproduction, inertness, pinned hwm (run: `probe_b1_replay_always_halt_wedge_inertness ... ok`)

Config: `snapshot_fencing: false`, `gap_timeout: 5 s` (default, per the CLOUD-261 recipe), isolated table.

```
B1 t=793.629835ms hole burned at seq 2; e2=3 e3=4 committed above
B1 t=7.793921376s halts after gap window (1 entries):
  [(5.808838857s, HaltInfo { subscriber_id: "probe262b1:wedge:e4d6882d…", held_below_sequence: 2, reason: GapUnproven })]
B1 TIMING: gap visible -> GapUnproven after 5.015s (gap_timeout = 5s)
B1 t=10.812548029s after e4=5 e5=6: head=6, position=1 (pinned at pre-hole seq 1? true)
B1 applied seq counts: [(1, 1), (3, 1), (4, 1)]
B1 inertness: post-wedge events delivered = 0 (expect 0); total halts = 1 (expect 1)
```

- **Wedge reproduced deterministically at 5c27432**, same shape as the CLOUD-261 probe at bed756b: `GapUnproven` at gap_timeout + one listener cycle (5.015 s vs 5.0 s), `held_below = 2` (the burned seq), delivered exactly once each: e1 (below hole), e2/e3 (above hole, inside the same fetched batch). Matches the live-batch firing site mod.rs:544-556 and the fail-closed refusal at subscriber_state.rs:366-374.
- **Inertness confirmed**: e4=5 and e5=6 published after the wedge were never delivered (0 deliveries), and no further halts fired (still exactly 1). Mechanism per code read at this rev: a wedged ReplayAlways subscriber is excluded from the shared batch (`wedged_now` filter, mod.rs:2258-2264) and from the P4b private fetch (ReplayAlways excluded, mod.rs:2116-2131 comment "the remedy is a fresh `subscribe()`, R9b"), so nothing ever fetches for it again.
- **Pinned in-memory hwm confirmed**: position = 1 = pre-hole seq while head = 6 (lag 5). The CLOUD-227 unbroken-prefix guard (mod.rs:3410-3424) keeps the hwm below the hole forever.

**Verdict: wedge reproduction at 5c27432 — verified; inert + pinned — verified.**

### B2 — readiness pin and resolvability (run: `probe_b2_readiness_pin_then_fresh_bus_resolves ... ok`)

Config: same wedge recipe (`snapshot_fencing: false`, `gap_timeout: 5 s`); wedged ReplayAlways+FailClosed subscriber the only registry entry on bus 1.

Pin phase (mechanism: `wait_until_all_caught_up` snapshots `subscriber_modes` at mod.rs:2971-2978 and polls every entry's position via `poll_until_all_at_or_past` mod.rs:2817-2841; a ReplayAlways position is the in-memory hwm, mod.rs:2735-2749):

```
B2 t=7.321211767s wedge established: hole=2, halt=GapUnproven held_below=2
B2 round 1: published seq 5, head=5, wedged position=1, wait_until_all_caught_up(2s) = false
B2 round 2: published seq 6, head=6, wedged position=1, wait_until_all_caught_up(2s) = false
B2 round 3: published seq 7, head=7, wedged position=1, wait_until_all_caught_up(2s) = false
```

3/3 rounds returned `Ok(false)` after their full 2 s timeout while fresh events kept publishing — the gate can never resolve while the wedged id stays registered: its hwm pins at 1 (CLOUD-227 guard, mod.rs:3410-3424) below every future head.

Resolvability phase — `bus.shutdown()`, then a **fresh bus instance on the same tables** (same events table, same history including the burned seq 2; fresh registry/hwm; wedged id never re-registered):

```
B2 leg2a: fresh bus, registry = healthy-at-head only; published seq 8, head=8; wait_until_all_caught_up(10s) = true
B2 leg2a: healthy-at-head position = 8 (head 8)
B2 leg2b: published seq 9; wait_until_all_caught_up(20s) = true
B2 leg2b delivery counts right after subscribe(): [(1, 1), (3, 1), (4, 1), (5, 1), (6, 1), (7, 1), (8, 1)]
B2 leg2b delivery counts settled:                [(1, 1), (3, 2), (4, 2), (5, 2), (6, 2), (7, 2), (8, 2), (9, 1)]
```

- **Leg 2a** (registry = one healthy Checkpointed+FailClosed subscriber planted at head — models the peers that were already caught up when the wedge hit): gate returned **true** immediately after one more publish; position = head = 8.
- **Leg 2b** (registry = one fresh ReplayAlways+FailOpen subscriber consuming the full holey backlog): the fail-open backstop crossed the burned seq 2 after the configured 5 s gap_timeout (bus log: "advancing … past 1 missing sequence(s) — seq 2 (5.014833856s)"), then the gate returned **true** with the subscriber at head.
- **Inference (not a measured unsubscribe call — no such API exists at 5c27432, mod.rs:1304 `type Projections<D> = Arc<Mutex<Vec<…>>>` has no removal path)**: bus 1 and bus 2 share identical tables, rows, and history; the only difference in the gate's inputs is registry membership. Removing the wedged id from the registry is exactly what flipped `wait_until_all_caught_up` from "never resolves" to "resolves". An `unsubscribe(subscriber_id)` that removes the observer from `projections` + the entry from `subscriber_modes`/`hwm` would restore resolvability on the live bus without a restart.

**Bonus finding (spec 0030 §4 double-delivery bug pinned at 5c27432, localized):** leg 2b's delivery counts show every above-hole sequence in the pre-existing backlog delivered **exactly twice** ((3..8) ×2), while seq 9 (published after `subscribe()`) delivered once. The ×1 snapshot taken immediately after `subscribe()` returned (before any new publish) proves the **first copy comes from the subscribe-time catch-up pass**; the second copy comes from the first live-path wake over the same window, whose fresh `processed_ahead` set (per-wake-seeded subscriber state, mod.rs:2080-2105) does not know what catch-up applied. The bug still reproduces at 5c27432 and its mechanism is catch-up-vs-live dedup, exactly the "`processed_ahead` guards the live path but not the catch-up pass" reading of spec 0030 §4 — and it is not "just the sequences adjacent above the hole": the whole above-hole backlog is re-delivered. Any P5 auto-heal (fresh id + replay) hits this on every heal over a hole.

**Verdict: readiness pin — verified (3/3 false rounds); resolvability without the wedged registry entry — verified (both legs true); unsubscribe-restore claim stated as inference.**

### B3 — wedge isolation: peer shared-batch progress continues past the wedge (run: `probe_b3_wedge_does_not_block_checkpointed_peer ... ok`)

Config: one bus, `snapshot_fencing: false`, `gap_timeout: 700 ms`; subscribers: wedged = ReplayAlways+FailClosed+Halt, peer = Checkpointed+FailOpen (a TimingObserver recording precise `on_event` entry times; its fail-open gap policy is its own affair and fires no halt — the measurement target is post-wedge progress).

```
[WARN] Gap timeout: advancing 'probe262b3:peer:1c221db9…' past 1 missing sequence(s) — seq 2 (1.004106216s) …
B3 t=2.457634097s wedge established (hole=2); peer has no halts
B3 post-wedge peer: 10/10 post-wedge events delivered; latency median 0.64 ms, p95 0.99 ms (publish->on_event entry)
B3 positions at head=14: peer position=14 (lag 0), wedge position=1 (lag 13)
B3 wedge applied seq counts (frozen): [(1, 1), (3, 1), (4, 1)]
```

- The wedged ReplayAlways subscriber froze exactly as in B1: applied set frozen at {1, 3, 4}, position pinned at 1, lag 13 at head 14.
- The Checkpointed peer kept consuming through the shared batch the whole time: **10/10 post-wedge events delivered, median publish→`on_event` latency 0.64 ms, p95 0.99 ms**, final position = head (lag 0). (A first B3 run under heavier shared-server load measured the same 10/10 outcome with median ≈10.6 ms and a 5.9 s DDL stall — the outcome is load-robust; the clean-run numbers above are the reported latencies.)
- Mechanism contrast with CLOUD-228 (head-of-line blocking): CLOUD-228 blocks peers because an in-flight `on_event` holds the observer's mutex, which the batch loop must re-lock per wake (mod.rs:2318-2327). A wedge holds no mutex — it is a state bit (`is_wedged`, subscriber_state.rs:196-201 region) that *excludes* the subscriber from the shared batch (`wedged_now` filter, mod.rs:2258-2264) and from the ReplayAlways P4b pass (mod.rs:2116-2131). The only peer-visible effect is that the wedged subscriber stops contributing to the shared floor (`compute_shared_floor`), so the floor floats up to the healthy peers — exactly what happened here.
- Note: during the pre-wedge gap window both subscribers kept receiving rows above the hole (e2/e3 delivered to both — the batch row-loop has no gap check); the peer then crossed the hole at its own fail-open backstop (1.004 s ≈ gap_timeout + one cycle) and never halted.

**Verdict: a ReplayAlways+Halt wedge does NOT block Checkpointed peers' shared-batch progress — verified (10/10 post-wedge deliveries, sub-millisecond latencies, no peer halt).**

### B4 — registry scan cost: per-wake delivery latency vs N parked subscribers (run: `probe_b4_registry_scan_cost ... ok`)

**Method.** For each N ∈ {0, 1, 10, 50} and 3 interleaved rounds (order 0,1,10,50 / 50,10,1,0 / 0,50,1,10; fresh isolated table + bus per config): N "parked" Checkpointed subscribers are registered with a planted checkpoint row at 1,000,000 (direct `epoch_event_bus_checkpoints` upsert before `subscribe()`, so the listener seeds their in-memory `contiguous_checkpoint` at 1,000,000 and the per-event check `event_seq <= contiguous_before` (mod.rs:527-529) skips every delivery — parked subscribers contribute the per-wake registry work (projections snapshot + init metadata pass over N observer mutexes, mod.rs:2036-2071; P4b filter; `compute_shared_floor`; on delivery wakes also the second O(N) priority-tagging pass, mod.rs:2318-2327, plus an O(rows) skip-loop per parked subscriber) but **zero `on_event` work**. One measured Checkpointed observer at position 0 records the precise `on_event` entry `Instant` per event. 3 warmup events, then 20 measured events, one per wake (40 ms spacing), each published by `INSERT … RETURNING` (NOTIFY fires at commit); latency = (Instant after the INSERT returns ≈ commit) → `on_event` entry, i.e. notify + wake + both O(N) scans + fetch + dispatch. Per-config-round medians/p95 (n = 20) reported as the mid of 3 round values with the round range.

```text
N=0:  median 0.652 ms (rounds 0.575 / 0.652 / 0.699)   p95 0.801 ms (0.801 / 0.731 / 0.865)
N=1:  median 0.685 ms (rounds 0.567 / 0.685 / 0.774)   p95 0.890 ms (0.651 / 0.942 / 0.890)
N=10: median 0.802 ms (rounds 0.670 / 0.802 / 0.818)   p95 0.906 ms (0.830 / 0.906 / 0.972)
N=50: median 1.082 ms (rounds 1.012 / 1.144 / 1.082)   p95 1.400 ms (1.137 / 1.400 / 1.457)
```

- **There is a real, small, monotone N-cost**: N=50's medians (1.012–1.144) sit above every N=0 round (0.575–0.699) in all 3 interleaved rounds; mid-vs-mid delta = **+0.430 ms median / +0.599 ms p95 for 50 parked subscribers ≈ 8.6–12 µs per parked subscriber per delivery wake**. This is an *upper bound* on the registry-scan cost: it includes the two O(N) observer-mutex passes plus each parked subscriber's O(rows) skip-loop and floor participation, not just the scan.
- **N ≤ 10 is inside run noise**: N=1's rounds (0.567–0.774) fully overlap N=0's (0.575–0.699); N=10 is +0.15 ms above N=0 at mid. The dominant latency term (~0.6 ms at N=0) is the notify → wake → fetch → dispatch pipeline, against which the scan is a microsecond-scale term at these N.
- **Honesty notes**: (a) shared server with a sibling prober active — the run log shows slow-statement blips up to 5.5 s on unrelated INSERTs/DROPs; these inflate occasional max samples (e.g. one 5.86 ms max at N=10 round 2) but the publish timestamp is taken after the INSERT returns, so publish-RTT stalls are excluded from the latency metric; medians/p95 are robust to them. (b) `cargo test` (debug profile), not `--release` — absolute values are conservative (overestimates); the per-subscriber *delta* is the transferable number. (c) parked subscribers skip via the checkpoint check; a parked subscriber whose observer mutex is held by a slow `on_event` would block the scan entirely (the deadlock note at mod.rs:2722-2724) — that hazard is separate from this measurement.

**Verdict: the O(N) per-wake observer scan is quantified at ≈8.6–12 µs per registered subscriber per delivery wake (upper bound incl. skip-loop work); irrelevant at N ≤ 10, +0.4–0.6 ms per wake at N = 50. Not a correctness risk — a bounded, linear scaling cost that a retire API would also remove.**

## Premises confirmed / overturned

**Confirmed:**

- CLOUD-261's wedge chain reproduces unchanged at 5c27432 (B1): GapUnproven at gap_timeout (+5.015 s measured vs 5 s configured), hwm pinned, post-wedge inertness, exactly one halt. Spec 0030's P5 gap class (ReplayAlways+FailClosed+Halt) is real on the current rev.
- `wait_until_all_caught_up` polls every registry entry's position (`subscriber_modes` snapshot at mod.rs:2971-2978 → `poll_until_all_at_or_past` mod.rs:2817-2841); a ReplayAlways position is the in-memory hwm (mod.rs:2735-2749), so a pinned hwm can never satisfy the gate (B2: 3/3 rounds false).
- Registry membership — not the wedge state itself — is what pins the gate: identical tables and history resolve the gate once the registry holds only healthy subscribers (B2 legs 2a/2b). This is the causal basis for the unsubscribe API's value.
- A wedged ReplayAlways subscriber holds no lock and starves no peer (B3): wedge exclusion (mod.rs:2258-2264, 2116-2131) is state-based, unlike CLOUD-228's in-flight `on_event` mutex hold.
- The brief's B4 premise (O(N) observer scan per wake, mod.rs:2036-2071) is real but small: ≈8.6–12 µs per registered subscriber per delivery wake (upper bound), measured.
- Spec 0030 §4's fresh-subscribe double-delivery bug **still reproduces at 5c27432** (B2 leg 2b: all above-hole backlog rows ×2), and the ×1 snapshot taken immediately after `subscribe()` localizes the first copy to the catch-up pass, the second to the first live wake over the same window with a fresh `processed_ahead` — the catch-up-vs-live dedup gap, and its blast radius is the whole above-hole backlog, not just adjacent sequences.

**Overturned / refined:**

- The brief's "per-wake locals … `subscriber_states`, `pending_checkpoints`, `checkpoint_cache`, `last_event_ids` (mod.rs:2046-2071, 2133+)" is **wrong for four of the seven maps**: `checkpoint_cache`, `pending_checkpoints`, `subscriber_states`, `last_event_ids` are **listener-lifetime** locals declared before the wake loop (mod.rs:1872-1885), with removal/reinsertion at batch boundaries. Only `sid_to_proj`, `replay_always_by_sid`, and the `projections_snapshot` are rebuilt per wake (mod.rs:2036-2071). Consequence for a future retire API: removing an observer from the registry Vec is *not* sufficient by itself — a lingering `subscriber_states` entry would keep participating in `compute_shared_floor` and (for Checkpointed wedges) P4b until some pruning step removes it; the retire design needs to prune the four long-lived maps (or gate them) in the same change.
- "Wait — the gate resolves trivially on an empty registry" (documented hazard mod.rs:2958-2963, confirmed by leg-2a's design): a retire API that can empty the registry makes `wait_until_all_caught_up` return `Ok(true)` with nobody driving anything. Spec 0031 must decide whether that hazard needs a guard when retire lands.

## Implications for spec 0031

1. **The retire API is the wedge remedy, mechanically.** B2 shows the readiness gate's unresolvability is pure registry membership. `unsubscribe(subscriber_id)` needs to: remove the observer from `projections` (mod.rs:1304 — today push-only), remove the entry from `subscriber_modes` (mod.rs:1429) and `hwm` (mod.rs:1420), and prune the four listener-lifetime maps (`subscriber_states`, `pending_checkpoints`, `checkpoint_cache`, `last_event_ids`, mod.rs:1872-1885) — the last part needs a cross-task signal (shared retired-id set consulted per wake), because plain registry removal does not clean those maps (see Premises, refined).
2. **Resolvability is restorable without re-subscribing anyone.** After retire, `wait_until_all_caught_up` resolves against the surviving healthy subscribers (B2 leg 2a: planted-at-head Checkpointed peer, gate true). No operator restart or bus rebuild is needed — the fresh-bus construction in B2 was a measurement device, not a required remedy.
3. **The P5 target class is confirmed exactly as spec 0030 scoped it**: ReplayAlways + FailClosed + `GapPolicy::Halt` wedged by `GapUnproven` (B1). Wedges are inert and lock-free (B3), so a retire is race-free against deliveries: nothing will ever deliver to the wedged observer again after exclusion begins, and a between-wakes retire leaves at most the current wake's Arc snapshot driving it (bounded, at-least-once semantics tolerate it — CLOUD-225 comment, mod.rs:2036-2041).
4. **Spec 0031's Part A (double-delivery fix) is validated as a hard prerequisite for P5's auto-heal**: B2 leg 2b confirms the bug still reproduces at 5c27432, localized to the catch-up-vs-live `processed_ahead` handoff, with blast radius = the entire above-hole backlog. Any auto-heal that re-subscribes over the wedge's own hole (which is, by construction, every P5 heal) double-applies the backlog into the fresh fold without that fix.
5. **Registry scan cost is not a reason to avoid registering subscribers, but retire bounds it**: +8.6–12 µs per subscriber per delivery wake (upper bound). At 50 subscribers that is +0.4–0.6 ms/wake — measurable, not pathological. A retire API also removes this term for retired subscribers, which is relevant for the P5 cap end-state (retire the chain, stop paying its scan).
6. **Document the empty-registry hazard** (mod.rs:2958-2963) wherever retire is specified: retiring the last subscriber turns `wait_until_all_caught_up` into a trivial `Ok(true)` — a monitoring blind spot unless warned or guarded.

## Open questions for Ruben

1. **Retire vs long-lived listener maps**: is the shared retired-id set consulted per wake the preferred mechanism to prune `subscriber_states`/`pending_checkpoints`/`checkpoint_cache`/`last_event_ids` (mod.rs:1872-1885), or should retire go through a listener command channel? (The four maps are listener-lifetime, not per-wake — plain registry removal leaks floor participation; see Premises.)
2. **Should retire of a Checkpointed subscriber delete or keep its `epoch_event_bus_checkpoints` row?** Keep is safer (audit; `release_halt` already writes rows for unregistered ids deliberately, mod.rs:~2627-2633), but a re-subscribe of the same id would resume from the retained row — is that the intended retire semantic?
3. **Does the empty-registry `Ok(true)` hazard (mod.rs:2958-2963) need a guard once retire can empty the registry** (e.g. `Err`/WARN when the gate is evaluated with zero subscribers), or is documentation enough?
4. **Post-retire API surface**: `subscriber_mode`/`subscriber_lag`/`wait_until_caught_up` on a retired id currently return `SubscriberNotFound` (registry-validated, mod.rs:1231, 2898-2907). Confirm that flipping from "pending forever" to a hard error is the desired observable behavior for dashboards polling a wedged id.
5. **P5 auto-heal vs the double-delivery blocker**: B2 leg 2b pins the bug at 5c27432 (whole above-hole backlog ×2, catch-up-vs-live dedup). Given spec 0030 §4 says "ticketed separately" and no ticket exists — should the fix land inside spec 0031 as a prerequisite part, per the prober's recommendation?

## Notes and residue

- All four batteries ran green on the first full pass after harness fixes (two harness bugs found and fixed during development: a `(stream_id, stream_version)` unique-key collision in the burn helper, and a latency computation that initially printed absolute arrival times instead of deltas — both corrected before any reported number was recorded).
- First B3 run occurred under heavy shared-server load (a 5.9 s `ALTER SEQUENCE` stall); its 10/10 peer-delivery outcome matched the clean run, only the latency scale differed. The clean-run numbers are the reported ones; the loaded run is disclosed above.
- Shared-server etiquette: only `probe_c262_a` was used; `probe_c262_b` (a sibling prober) and all `test_*`/`catacloud*` databases were never touched. All experiment tables were per-test isolated tables, dropped by the harness at the end of each test; the scratch DB is dropped at probe end.
- The temporary harness `epoch_pg/tests/probe_c262_a.rs` is deleted after the run; `git status` must show only this report file from this probe.
