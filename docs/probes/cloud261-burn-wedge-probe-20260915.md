# Probe Report: CLOUD-261 sequence-burn wedge for ReplayAlways subscribers

- Date: 2026-09-15
- Repository: `/root/code/epoch-worktrees/cloud-261` (epoch, Rust CQRS/event-sourcing framework)
- Branch / base commit: `bed756b` — "refactor(core,mem,pg): CLOUD-242 post-review fixes — trait invariant docs, mem dedup, test repeatability" (`git log --oneline -1`), clean tree at probe start. All cited line anchors were re-verified against this rev and hold.
- Mode: **probe** (temporary integration-test harness under `epoch_pg/tests/`, deleted afterwards; scratch Postgres DB `probe_cloud261_wedge`, dropped afterwards)
- Infrastructure: Postgres 18.4 container at 127.0.0.1:5457; migrations applied to the scratch DB via `epoch_pg::Migrator`; all bus experiments ran on per-test isolated events tables (own `global_sequence` sequence), so no burn ever touched a shared table.

## Summary of verdicts

| Q | Verdict |
|---|---------|
| Q1 | **Verified** — a rolled-back event INSERT permanently burns the nextval: reserved seq 5, row absent, sequence `last_value=5`, next committed insert got 6. |
| Q2 | **Verified** — one deterministic burn wedges a live ReplayAlways+FailClosed subscriber: `GapUnproven` fired once **5.05 s after the gap became visible** (gap_timeout=5 s), HWM pinned at the pre-hole seq while head advanced 1→6, delivery of post-wedge events stopped. |
| Q3 | **Verified (fencing off)** — a row committed AT the skipped seq is **never detected**: not delivered, position stays pinned, no new halt, no fence to prove; the wedged ReplayAlways subscriber is fetched by nothing. |
| Q4a | **Verified** — a fresh ReplayAlways+FailClosed `subscribe()` delivers events above the hole but **re-wedges at it** (`GapUnproven` again), pins below the hole, and **re-delivers two above-hole events twice** (delivery counts seq 3 ×2, seq 4 ×2). |
| Q4b | **Verified** — `release_halt` is reachable (`pub async fn`, mod.rs:2290), returns `Ok(())`, writes a checkpoint row, fires `HaltReason::Released` — but **does not resume** a ReplayAlways subscriber (position stays pinned; no delivery resumes). |
| Bonus | **Verified** — with default `snapshot_fencing: true` and a plain `nextval` burn (no long-lived txn), there is **no wedge**: position crossed the hole within ~1.1 s via `FenceCleared`, 0 halts. |

Every claim below was executed this session; command output is quoted verbatim (test names, timestamps and all). The temporary harness `epoch_pg/tests/cloud261_probe_tests.rs` (5 `#[tokio::test] #[serial]` tests, run with `DATABASE_URL='postgres://postgres:postgres@127.0.0.1:5457/probe_cloud261_wedge' EPOCH_REQUIRE_DB=1 cargo test -p epoch_pg --test cloud261_probe_tests -- --nocapture --test-threads=1`) was deleted after the run; final `git status --porcelain` shows no residue from this probe. Scratch DB dropped (`DROP DATABASE probe_cloud261_wedge` → confirmed).

Line-anchor re-verification (all confirmed against `bed756b`):

- GapUnproven fires from the live-batch path only: `epoch_pg/src/event_bus/mod.rs:544-556` — `if let Some(refused_seq) = outcome.backstop_refused { fire_on_halt(&config, &subscriber_id, refused_seq, HaltReason::GapUnproven).await }`.
- Fence condition: `epoch_pg/src/event_bus/subscriber_state.rs:341-344` — `if let (Some(snap), Some(fence)) = (snapshot, fence_xmax) && snap.xmin >= fence` → `SkipReason::FenceCleared`; FailClosed refusal at `subscriber_state.rs:366-374` sets `halt_fired` + `backstop_refused = Some(next)`.
- `gap_timeout` default 5 s: `epoch_pg/src/event_bus/config.rs:388` — `gap_timeout: Duration::from_secs(5)`.
- CLOUD-227 unbroken-prefix guard for ReplayAlways HWM: `mod.rs:3410-3424` — `if event_global_seq != *contiguous + 1 { return; }`.
- P4b self-heal excludes ReplayAlways: `mod.rs:1884-1893` — "…`ReplayAlways` wedges are out of P4b's scope (no persisted checkpoint row to re-seed from; the remedy is a fresh `subscribe()`, R9b)" and the filter at `mod.rs:1891`: `s.is_wedged() && !replay_always_by_sid.get(*sid).copied().unwrap_or(false)`.

## Q1: Does a rolled-back event INSERT permanently burn the global_sequence?

**Verdict:** Yes — the reserved value is consumed and never reusable; the next committed insert skips it.

**Method:** Against the scratch DB with migrations applied, on `epoch_events` itself: read `max(global_sequence)`; `BEGIN`; `INSERT` an event row without specifying `global_sequence` (so the column default `nextval(...)` fires); `ROLLBACK`; then a normal committed `INSERT`. Sequence name resolved via `pg_get_serial_sequence`.

**Evidence (verbatim from the test run, `probe_q1_burn_is_permanent ... ok`):**

```
Q1 sequence backing epoch_events.global_sequence: public.epoch_events_global_sequence_seq
Q1 [+127.613449ms] max(global_sequence) before burn: 0
Q1 [+129.68926ms] seq reserved inside tx then ROLLED BACK: 5
Q1 rows at burned seq after rollback: 0; sequence last_value: 5
Q1 [+135.285559ms] next committed insert got global_sequence: 6
```

(The table was empty — `max=0` — but the sequence had already advanced to 5 from burns in earlier aborted harness attempts, itself additional evidence: values 1–4 burned in prior runs are equally gone. The test asserts `s_next == s_burned + 1` and `rows_at_burned == 0` and passed.)

**Confidence:** verified.

**Implications:** The premise of CLOUD-261 option A (gaps from rolled-back transactions are real, permanent, and deterministic to manufacture) is confirmed exactly as the issue states.

## Q2: Does one mid-stream gap wedge a live ReplayAlways + fail-closed subscriber?

**Verdict:** Yes — GapUnproven fires once ≈ gap_timeout (5.05 s observed vs 5 s configured) after the gap becomes visible; the HWM pins below the hole while the head advances; delivery of anything published after the halt stops.

**Method:** Isolated events table + own sequence; `ReliableDeliveryConfig { snapshot_fencing: false, gap_timeout: 5 s (default), events_table, on_halt: timestamped callback }`; bus started (`setup_trigger` + `start_listener`); a `ReplayAlways` + `FailClosed` projection subscribed; e1 published and delivery awaited (position = `head_sequence() − subscriber_lag()`, i.e. the ReplayAlways in-memory HWM per `mod.rs:2449-2459`); then the deterministic burn — `INSERT` inside an open transaction to claim seq 2, commit e2=3 and e3=4 above it, roll the hole transaction back; wait 8 s; publish e4=5, e5=6; wait 3 s. `snapshot_fencing: false` is the "fence stays unproven" leg of the issue's chain (see Bonus for the default-fencing behavior).

**Evidence (verbatim, `probe_q2_q3_gap_wedge_then_late_materialization ... ok`):**

```
Q2 [+368.982993ms] e1 committed at seq 1; position after delivery: 1
Q2 [+419.680078ms] gap burned at seq 2; e2=3 e3=4 committed above it
Q2 [+8.421022836s] halts after gap_timeout window (1 entries):
  HALT at test+5.435259949s: subscriber=probe261:ra-fc:92d87a16-... held_below=2 reason=GapUnproven
Q2 TIMING: gap became visible -> GapUnproven fired after 5.053757791s (gap_timeout = 5 s)
Q2 [+11.433636813s] after publishing e4=5 e5=6: head=6, position=1 (pinned below hole 2? true)
Q2 halts after e4/e5 (1 entries):            [still exactly 1 halt — no repeats, no recovery]
Q2 applied events (total 3): e1=true e2=true e3=true e4=false e5=false
```

Observed chain, exactly as CLOUD-261 describes: gap seen → fence unavailable (fencing off) → backstop would fire at `gap_duration > gap_timeout` but FailClosed refuses (`subscriber_state.rs:366-374`) → `on_halt(GapUnproven)` from the live-batch path (mod.rs:544-556) at gap_timeout + one listener cycle (~50 ms) → subscriber `is_wedged()` (`subscriber_state.rs:174-182`: FailClosed ∧ `gap_first_seen` has `halt_fired`) → excluded from the shared fetch floor (mod.rs:1955-1964) **and** from P4b (mod.rs:1891) → no fetches at all → HWM (CLOUD-227 guard, mod.rs:3410-3424) and delivery both frozen. Two nuances measured precisely: (a) e2/e3 — the rows that shared the fetched batch with the gap — *were* delivered exactly once before the halt (the batch row-loop has no gap check; dedup is `processed_ahead`, mod.rs:359); (b) everything published after the wedge (e4/e5) is never delivered.

**Confidence:** verified.

**Implications:** The failure chain is real and deterministic — no load race needed, one burn suffices. "Delivery stops" needs the precise reading above: it stops for post-wedge events; the wedge is permanent because a wedged ReplayAlways subscriber is driven by nothing (shared loop excludes wedged, P4b excludes ReplayAlways).

## Q3: Late materialization — is a row inserted AT the skipped sequence detected?

**Verdict:** No — nothing detects it. The row commits and sits visible in the table; it is never delivered, the position stays pinned, no halt changes, the subscriber stays halted. (Fencing was off in this run, so there was no fence to prove; see Bonus + Notes for what fencing does and does not change.)

**Method:** Continuation of the Q2 wedge: `INSERT` a row with explicit `global_sequence = 2` (the burned value), then publish e6=7 above it, wait 4 s (≈4 listener cycles), and re-read everything (position, applied events, halt log, row presence).

**Evidence (verbatim, same test, `... ok`):**

```
Q3 [+11.442398878s] late row inserted AT skipped seq 2 (id c2600be8-90f0-42aa-b07e-2050ba97acc9); e6=7 published above
Q3 [+15.446775615s] halts after late row + e6 (1 entries):      [unchanged — still only the one GapUnproven]
Q3 observed: late row present=1; position=1 (head=7); applied total=3; late_row_delivered=false; e6_delivered=false; total halts=1
```

The mechanism, from code read this session: a wedged ReplayAlways subscriber is excluded from the shared batch (`wedged_now.contains(sid)` filter, mod.rs:2074-2076) and from P4b's private fetch (mod.rs:1891); with it the only subscriber, `compute_shared_floor` returns `None` and the shared fetch is skipped entirely (mod.rs:1955-1964: "`None` means every subscriber is wedged … the shared fetch is skipped for this cycle"). No query ever reads seq 2 again, so `visible_seqs` never contains it and the fence/gap machinery in `advance_contiguous_checkpoint` is never re-evaluated for this subscriber. The fence question as asked: with `snapshot_fencing: false` the fence branch is unreachable (`if let (Some(snap), Some(fence)) = (snapshot, fence_xmax)` — both `None`); the late row also cannot "resolve" anything through the fence because no snapshot is ever taken for this subscriber again.

**Confidence:** verified for the fencing-off configuration as observed. Not run (labeled, not extrapolated): the same experiment under `snapshot_fencing: true` with the subscriber already wedged — code reading says the outcome is identical (the exclusion at mod.rs:1891 is fetch-level, not fence-level), but that variant was not executed; see Notes.

**Implications:** This is the core safety datapoint for option A: a "late-materializing row at a skipped seq" is currently **invisible** to a wedged ReplayAlways subscriber — no rebuild trigger, no detection, no fence resolution. Any SkipAfterBackstop design must add its own detection; nothing existing will catch it.

## Q4a: Does a fresh subscribe() deliver past the hole?

**Verdict:** Partially, at a cost — a fresh ReplayAlways+FailClosed subscriber DOES receive the events above the hole, but it re-wedges at the hole itself (`GapUnproven` again), its position pins below the hole, and two of the above-hole events were **delivered twice** to the same subscriber.

**Method:** Fresh isolated table + bus (`snapshot_fencing: false`, `gap_timeout: 700 ms` for speed); burn the hole first (e1=1, claim+rollback seq 2, e2=3, e3=4 committed); *then* subscribe a new ReplayAlways+FailClosed projection; publish e4=5, e5=6; wait 4 s; record delivery counts per `global_sequence` (join applied event ids back to the table) plus position, checkpoint row, and halts.

**Evidence (verbatim, `probe_q4a_fresh_subscribe_past_hole ... ok`):**

```
Q4a [+317.597912ms] hole burned at 2; e1=1 e2=3 e3=4 exist
Q4a delivery counts by global_sequence (seq, times_delivered): [(1, 1), (3, 2), (4, 2), (5, 1), (6, 1)]
Q4a [+4.337260866s] fresh-subscriber halts (1 entries):
  HALT at test+1.332782737s: subscriber=probe261:fresh:a8f81ab1-... held_below=2 reason=GapUnproven
Q4a observed: position=1 (head=6, hole=2); checkpoint_row=None; applied total=7
Q4a applied: e1=true e2=true e3=true e4=true e5=true
```

So: 7 deliveries for 5 events; seqs 3 and 4 (immediately above the hole) delivered twice each. Plausible mechanism (partial, from code shape, not separately isolated): the subscriber's initial catch-up pass (listener-side, `catch_up_from_checkpoint` invoked for a newly seen subscriber at mod.rs:1603) delivers rows above the hole as it scans (its prefix counter stops at the hole via the CLOUD-227 guard but delivery of scanned rows still happens), and the subsequent live batch — with a freshly seeded, empty `processed_ahead` — re-delivers what the catch-up already applied; the `processed_ahead` dedup (mod.rs:359) guards only within the live path's state. I did not isolate which pass delivered which copy; the counts themselves are verified.

**Confidence:** verified for the observed numbers (delivery counts, re-wedge, pinned position, no checkpoint row). Partial for the internal mechanism of the duplicates.

**Implications:** "The remedy is a fresh `subscribe()`" (mod.rs:1884-1893) is not a clean remedy: it restores delivery of current events but (a) immediately re-wedges at the same hole with its own GapUnproven, (b) re-pins below the hole, and (c) can double-deliver above-hole events — poisonous for exactly the fold-style ReplayAlways projections option A targets. Any option A/B evaluation should treat fresh-subscribe as a workaround with known defects, not a recovery path.

## Q4b: Is release_halt reachable, and does it resume the wedged subscriber?

**Verdict:** Reachable and callable — `pub async fn release_halt(&self, subscriber_id: &str, past_sequence: u64)` (mod.rs:2290) — and on a wedged ReplayAlways subscriber it returns `Ok(())`, writes a checkpoint row, and fires `HaltReason::Released`, but it does **not** resume delivery; the position stays pinned and later events are not delivered.

**Method:** Reproduce the Q2 wedge (`gap_timeout: 700 ms`), confirm `GapUnproven`, call `bus.release_halt(&fc_id, 2).await`, read the checkpoint row, publish e4=5, wait 3 s, re-read position/applied/halts.

**Evidence (verbatim, `probe_q4b_release_halt_on_wedged_replay_always ... ok`):**

```
Q4b halts before release (1 entries):
  HALT at test+1.392602521s: subscriber=probe261:release:72d40a25-... held_below=2 reason=GapUnproven
[WARN] Operator release: advanced subscriber 'probe261:release:72d40a25-...' past held sequence 2 (was 0). Any sequence at or below 2 the subscriber never finished is now skipped; delivery resumes from 3.
Q4b [+2.899934491s] release_halt(sub, 2) result: Ok("Ok(())")
Q4b checkpoint row after release_halt: Some(2)
Q4b halts after release + e4 (2 entries):
  HALT at test+1.392602521s: ... reason=GapUnproven
  HALT at test+2.899931811s: ... reason=Released
Q4b observed: position=1 (head=5, hole=2); e1=true e2=true e3=true e4=false (total 3)
```

The WARN text ("delivery resumes from 3") is written by `release_halt` (mod.rs:2325-2331) but is false for ReplayAlways: the row it writes into `epoch_event_bus_checkpoints` is never read back, because the ReplayAlways live path routes advancement to the in-memory HWM and never re-seeds from the persisted row (mod.rs:709-714, mod.rs:742-745: "Gated on `!replay_always` so a ReplayAlways subscriber writes no row"), and the only runtime re-seed consumer (P4b, mod.rs:1897-1901) explicitly excludes ReplayAlways. Contrast: for a **Checkpointed** fail-closed subscriber the same call does resume in-process — proven by the existing suite test `test_wedged_gap_does_not_starve_peer_and_release_resumes` (`epoch_pg/tests/pgeventbus_integration_tests.rs:8718-8880`, run in this tree's suite; its R14 leg polls the released subscriber to the tail).

**Confidence:** verified.

**Implications:** For ReplayAlways subscribers there is currently no operator recovery path at all: release_halt is a silent no-op on the live subscriber (its log line actively over-promises), and a fresh subscribe re-wedges (Q4a). This strengthens the case that CLOUD-261's gap ("the remedy is a fresh subscribe()") leaves ReplayAlways wedges unrecoverable in place.

## Bonus (probe of the issue's premise under default config): does the fence stay unproven?

**Verdict:** With default `snapshot_fencing: true` and a plain `nextval` burn (no long-lived transaction), there is **no wedge**: the fence proves the burn via `FenceCleared` and the position crosses the hole within ~1.1 s, with zero halts.

**Method:** Default config (`snapshot_fencing: true`, `gap_timeout: 5 s`); ReplayAlways+FailClosed subscriber; deterministic sentinel burn (`SELECT nextval(...)` — sequence slot consumed, no row, no open transaction); e2=3, e3=4 above; poll position for 15 s.

**Evidence (verbatim, `probe_bonus_fencing_on_plain_burn_selfheals ... ok`):**

```
BONUS [+460.953665ms] burn (plain nextval) at 2; e2=3 e3=4 above
BONUS [+1.564836858s] observed: position=4 (hole=2, s3=4); halts=0; e2_delivered=true e3_delivered=true (total 3)
```

This matches the existing suite test `test_fail_closed_gap_refusal_and_fence_cleared_recovery` (`epoch_pg/tests/pgeventbus_integration_tests.rs:8432-8580`), whose doc states the recovery leg: "rolling back the held transaction (so the sequence provably never existed) clears the snapshot fence, and the subscriber's checkpoint then advances past the gap via `FenceCleared`". The wedge of Q2 therefore requires the fence to be *unavailable or unproven* — fencing disabled, or `xmin` pinned below `fence_xmax` past `gap_timeout` (e.g. long-running in-flight transactions on a production instance). I did not run the xmin-pinned variant (deterministically pinning `xmin` past 5 s needs a deliberately held write transaction; time-boxed out — see Notes).

**Confidence:** verified for the plain-burn/idle-DB case.

**Implications:** CLOUD-261's failure chain is accurate but conditional: on an idle DB with default fencing, a pure burn self-heals; the wedge reproduces when fencing is off (as run in Q2–Q4) or when fence `xmin` cannot advance past `fence_xmax` within `gap_timeout` — the latter being precisely the "in-flight writer" safety question option A raises. Option A's SkipAfterBackstop must be evaluated against both conditions.

## Notes (adjacent observations, not chased)

1. **Untested variant:** the Q3 late-materialization experiment under `snapshot_fencing: true` with an already-wedged ReplayAlways subscriber (and under an `xmin`-pinned fence). Code reading (mod.rs:1891 — P4b exclusion is fetch-level) predicts the same "nothing detects it" outcome, but this was not executed.
2. **Q4a duplicate deliveries** ((3,2),(4,2)) look like a catch-up-pass vs live-path dedup gap for a position-pinned ReplayAlways subscriber. If real, it is a distinct correctness issue from CLOUD-261 (double-apply for fold-style projections after any fresh subscribe over a hole) — worth its own investigation; not fixed or further chased here per prober discipline.
3. **`release_halt`'s WARN over-promise** for ReplayAlways ("delivery resumes from 3") is a small doc/logging defect worth folding into whatever CLOUD-261 lands.
4. **Residue check:** none. `git status --porcelain` after cleanup shows only the untracked `docs/probes/` directory containing this report (plus sibling agents' report files, untouched). The harness file `epoch_pg/tests/cloud261_probe_tests.rs` was deleted; the scratch database `probe_cloud261_wedge` was dropped; no shared tables (`catacloud`, `catacloud_template`, `test_*`) were read or written. The `epoch_events` table inside the (now dropped) scratch DB had probe rows deleted by the harness itself before the drop.
