# Probe Report: CLOUD-261 option B — sequence-burn semantics, allocation cost, crash bounds, ordering caveat

- Date: 2026-09-15
- Repository: `/root/code/epoch-worktrees/cloud-261` (epoch Rust CQRS/event-sourcing framework)
- Branch/worktree: cloud-261 worktree; base commit `bed756bc22f76bdd7e8ea68cc436f82b635fc431` (HEAD, `git status --porcelain` clean at start)
- Mode: **probe** — zero repository mutation; all scratch under `/tmp/cloud261_alloc/`, all SQL against scratch database `probe_cloud261_alloc` (created and dropped this session; `catacloud`/`catacloud_template`/`test_*` never touched)
- Infrastructure: Postgres 18.4 in container `cloud-259-postgres-1`, host port 5457 (container-internal 5432), default durability settings (fsync on)

## Summary of verdicts

- **Q1 Burn semantics:** nextval burns all 10 values of a rolled-back 10-row txn (next value 12 after anchor 1); per-txn counter allocation burns 0 (counter reverts to 1, next writer gets 2); cached 64-block with 5 used burns the unused 59 (next writer gets 67). All quoted with actual values.
- **Q2 Contention:** counter-row UPDATE per txn (K=1) is **3.5–4.4× slower** than column-DEFAULT nextval (≈290 vs ≈1030–1270 tps, p95 ≈60 ms vs ≈7–11 ms at 8 clients × 100 single-row commits); cached 32-blocks track nextval (within run-to-run noise). Measures SQL shape, not epoch's insert path.
- **Q3 Crash bounds:** `pg_terminate_backend` during an uncommitted per-txn counter allocation leaves the counter unchanged (10→0 burn, fresh writer gets the same seq 1; no committed value reused); terminating a session holding a committed 64-block (5 used) burns the 59-value remainder (next writer gets 66 past block 2..65), never reuses.
- **Q4 Ordering caveat:** every bus read is a `global_sequence`-ordered cursor and the gap fence pairs seqs with xmins under the premise that a missing seq's writer, if any, was already in-flight (had an xid) at detection — a premise nextval guarantees but cached blocks break: the future filler of a missing seq may have no xid yet, so a fence-"proven" gap can later materialize rows from below. Files checked listed in Q4; no fix designed.

**Line-anchor check (CLOUD-261 vs `bed756b`):** all anchors verified in place — `subscriber_state.rs:337-363` fence (check `snap.xmin >= fence` at line 350), `mod.rs:544-556` GapUnproven firing (553), `mod.rs:1884-1893` ReplayAlways P4b exclusion ("the remedy is a fresh `subscribe()`" at 1885), `mod.rs:3410-3424` CLOUD-227 unbroken-prefix guard (comment at 3416), `config.rs:388` `gap_timeout: Duration::from_secs(5)` (lines 384–392). No drift.

---

## Q1: Burn semantics — (a) per-row nextval, (b) per-txn counter, (c) per-writer cached block

**Verdict:** (a) a rolled-back txn burns **every** nextval it consumed (10/10); (b) per-txn counter allocation burns **zero** and hands the next committed writer a contiguous value; (c) a crashed block holder burns the **entire unused remainder** (59 of 64 after using 5).

**Method:** Scratch tables in `probe_cloud261_alloc` mimicking the real shapes: `q1_nextval (seq BIGINT DEFAULT nextval('q1_seq') PRIMARY KEY, tag text)` mirrors `epoch_events.global_sequence BIGINT DEFAULT nextval('epoch_events_global_sequence_seq')` (m002_add_global_sequence.rs:31–42) and epoch's insert path (`INSERT ... VALUES (...) RETURNING global_sequence`, the column omitted so the DEFAULT fires — event_store.rs:145–153 and 457–464). Counter variant mirrors option B(i): `UPDATE counter SET val = val + K RETURNING val` inside the txn. Scripts `/tmp/cloud261_alloc/q1a.sql`, `q1b.sql`, `q1c_a.sql`/`q1c_b.sql` run via host psql.

**Evidence:**

(a) Per-row nextval — insert 10 rows, ROLLBACK:

```text
--- inside txn: INSERT ... SELECT generate_series(1,10) ... RETURNING seq
 seq
-----
   2
   3
   ...
  11
(10 rows)
ROLLBACK
--- after ROLLBACK, committed rows:
 seq |  tag
-----+--------
   1 | anchor
(1 row)
--- what the sequence hands out next:
 next_value
------------
         12
```

→ values 2..11 (10 values) burned; anchor 1 committed; next value 12.

(b) Per-txn counter — allocate exactly 10, insert rows 2..11, ROLLBACK:

```text
UPDATE q1_counter SET val = val + 10 WHERE name = 'events' RETURNING val AS allocated_upto;
 allocated_upto
----------------
             11
ROLLBACK
--- counter after ROLLBACK:
  name  | val
--------+-----
 events |   1
--- next committed writer allocates K=1:
 allocated_upto
----------------
              2
COMMIT
--- all committed rows now:
 seq |       tag
-----+------------------
   1 | anchor
   2 | committed_writer
```

→ counter reverts to 1 (zero burn); the next committed writer gets val = 2, contiguous after the anchor.

(c) Per-writer cached block — session A reserves 64 (counter 2 → 66, block 3..66), commits 5 rows in five small txns (seqs 3..7), then exits (disconnect):

```text
=== Q1(c) session A: reserve block of 64 from counter, use 5 across small txns, then DISCONNECT ===
UPDATE 1
block reserved: 66-63 .. 66
[5 × BEGIN/INSERT/COMMIT → rows 3..7]
--- session A disconnects NOW while still holding seqs 8..66 unused
### session A exited (disconnected) ###
=== Q1(c) session B: after A disconnected, a fresh writer allocates ===
UPDATE 1
 seq |       tag
-----+------------------
   1 | anchor
   2 | committed_writer
   3 | block_user
   ...
   7 | block_user
(7 rows)
 counter_now
-------------
          67
```

→ fresh writer handed seq 67 (`counter_now = 67`); seqs 8..66 — the 59 unused values of the committed block — are burned forever. (Note: psql `\echo` interpolated the variable labels imperfectly; the values are quoted from the `RETURNING` results and final SELECTs, which are unambiguous.)

**Confidence:** verified (all values quoted from live SQL output this session).

**Implications:** confirms the issue's premise numerically: nextval's burn-per-rollback is exactly the rolled-back row count, option B(i) eliminates it entirely at commit time, and B(ii)'s burn is bounded by the block remainder and occurs only on writer death — not on ordinary rollback (Q1c rollbacks of individual row-insert txns inside a held block would burn nothing; only the block tail is at risk).

---

## Q2: Contention microbenchmark — 8 clients × 100 single-row-insert commits, three variants

**Verdict:** counter-row UPDATE per txn (K=1) is the expensive shape — **≈290 tps vs ≈1030–1270 tps for nextval (3.5–4.4× slower), p95 ≈60 ms vs ≈7–11 ms**; cached 32-blocks perform like nextval (within noise). Caveat: this measures SQL statement shape, not epoch's real insert path.

**Method:** Three tables shaped like `epoch_events` (uuid id, text tag, timestamptz created_at DEFAULT now(), sequence/seq column, one btree index on the seq column — mirroring `idx_epoch_events_global_sequence`):

- **v1 (column-DEFAULT nextval):** `INSERT INTO bench_v1 (id, tag) VALUES (gen_random_uuid(), 'x') RETURNING global_sequence;` — exactly epoch's statement shape (DEFAULT nextval, column omitted, RETURNING).
- **v2 (counter per txn, K=1):** `WITH r AS (UPDATE bench_counter SET val = val + 1 WHERE name='events' RETURNING val) INSERT INTO bench_v2 (seq, id, tag) SELECT val, gen_random_uuid(), 'x' FROM r RETURNING seq;`
- **v3 (cached 32-blocks):** pgbench client-side cache; refill `UPDATE bench_counter SET val = val + 32 ... RETURNING val AS hi \gset` when the local cache is empty, then `INSERT INTO bench_v3 (seq, id, tag) VALUES (:cache_hi - :cache_remaining, ...)`.

pgbench 18.4 inside the container, `-c 8 -t 100 -n -l --log-prefix=...` (1 thread, 8 clients), after a discarded warmup; p95 computed from the per-transaction latency logs (column 3, µs, `sort -n`, index ⌈0.95·N⌉ = 760 of 800). Two measured rounds to expose run noise. Scripts: `/tmp/cloud261_alloc/b1.sql`, `b2.sql`, `b3.sql`, `run_bench.sh`.

**Evidence (measured runs, quoted):**

Round 1:

```text
v1: number of transactions actually processed: 800/800  latency average = 7.797 ms  tps = 1025.985652
v2: number of transactions actually processed: 800/800  latency average = 27.263 ms  tps = 293.438208
v3: number of transactions actually processed: 800/800  latency average = 7.554 ms  tps = 1059.056989
p95: v1 n=800 median=6283us p95=10667us max=48411us
     v2 n=800 median=21398us p95=59719us max=172291us
     v3 n=800 median=6387us p95=11902us max=57717us
```

Round 2 (tables truncated first, counter reset):

```text
v1: 800/800  latency average = 6.285 ms  tps = 1272.825338
v2: 800/800  latency average = 27.641 ms  tps = 289.422826
v3: 800/800  latency average = 9.561 ms  tps = 836.721183
p95: v1 n=800 median=6103us p95=6895us max=11610us
     v2 n=800 median=22698us p95=61472us max=101511us
     v3 n=800 median=6385us p95=19121us max=140233us
```

Correctness invariants: every variant produced only distinct seq values (round 1 end-state: `bench_v1|1000|1000|1|1000`, `bench_v2|1000|1000|65|1064`, `bench_v3|1020|1020|1|2316` rows/distinct/min/max). v3 round 2: 800 rows, all distinct, seqs 1..996, counter final **1024** = 8 clients × ⌈100/32⌉ refills × 32 → **224 of 1024 allocated values (21.9%) never attached to a row** — pure ⌈100/32⌉ block-tail waste with *zero crashes*, confirming the burn model of variant (ii) quantitatively.

**Caveats (honest):**

- This is SQL shape, not epoch: epoch inserts 12 columns with sqlx bindings, fires LISTEN/NOTIFY triggers, and runs inside the tokio stack. Relative shape cost is what transfers, not absolute numbers.
- The box is shared with three other agents' Postgres activity; run-to-run noise is real (v3: 1059 → 837 tps between rounds; v1: 1026 → 1273). v2's deficit (≈290 tps both rounds, p95 ≈60 ms both rounds) is far outside that noise.
- Mechanism, for interpretation: nextval's internal lock is held for nanoseconds and released before commit; the counter-row UPDATE holds its row lock **until COMMIT**, so at 8 clients v2 effectively serializes whole transactions — the p95 (~60 ms ≈ 8 × per-txn latency) shows the queue. v3 only pays the counter UPDATE on ~1 in 32 txns (25 refills per 800), and those refills are exactly the shape v2 pays on *every* txn.

**Confidence:** verified for the shapes measured (two independent runs, invariants checked); partial as a proxy for epoch's production path.

**Implications:** the issue's flagged cost is real and concentrated in variant B(i): per-txn exact allocation converts a nanosecond-scale lock-free sequence into a commit-duration-serialized row lock — ~3.5–4.4× throughput loss at only 8 writers, growing with concurrency. Variant B(ii) (cached blocks) is cheap on the hot path; its costs are the crash-burn bound (Q1c/Q3b) and the ordering property examined in Q4.

---

## Q3: Crash bounds — pg_terminate_backend mid-allocation

**Verdict:** (a) terminating a session inside an uncommitted per-txn counter allocation leaves the counter unchanged (zero burn) and no committed value is ever reused — the next writer gets the freed number. (b) terminating a session holding a *committed* 64-block burns the unused remainder (59 here) and those values are never handed out again.

**Method:** Fresh tables `q3_counter`/`q3_rows`; victim psql sessions run in background (log captured to `/tmp/cloud261_alloc/q3a_victim.log` / `q3b_victim.log`), located via `pg_stat_activity` (datname-filtered), killed with `pg_terminate_backend(pid)`; state checked before/after; then a fresh committed writer allocates K=1.

**Evidence:**

(a) Victim txn: `BEGIN; UPDATE q3_counter SET val = val + 10 RETURNING val AS allocated_upto` → `10` (provisional seqs 1..10; `INSERT 0 10` uncommitted), then `pg_sleep(120)`:

```text
--- state BEFORE terminate:
counter_val|0
rows_committed|0
 terminated
------------
 t
psql:q3a_bg.sql:8: FATAL:  terminating connection due to administrator command
--- state AFTER terminate:
counter_val|0
rows_committed|0
--- fresh committed writer allocates K=1:
writer_got_seq|1
final_rows|1=after_crash
counter_final|1
```

→ counter never moved (0 before, 0 after); the 10 provisional values evaporated; the fresh writer received seq 1 — the same number the dead txn had provisionally used — and it is the only committed row. Zero burn; no *committed* value reused.

(b) Victim: committed `UPDATE q3_counter SET val = val + 64 RETURNING val AS block_end` → `65` (block 2..65), committed 5 rows (seqs 2..6), terminated while idle in `pg_sleep`:

```text
--- state BEFORE terminate (reservation committed):
counter_val|65
committed_rows|1,2,3,4,5,6        (seq 1 is Q3(a)'s 'after_crash' row)
 terminated
------------
 t
psql:q3b_bg.sql:22: FATAL:  terminating connection due to administrator command
--- state AFTER terminate:
counter_val|65
committed_rows|1,2,3,4,5,6
--- fresh writer allocates K=1 after the crash:
writer_got_seq|66
```

→ the committed reservation survives the kill; seqs 7..65 (59 values) are burned permanently; the next writer got 66, not 7 — no reuse of burned values.

**Confidence:** verified (both kills performed live; all values quoted).

**Implications:** crash bounds match the design intent: B(i) burns zero on crash *of an uncommitted txn* (the only burn left is a crash between commit and row insert, which for B(i) is the same txn — hence zero), while B(ii) burns exactly the unused block remainder per crashed writer. Note the asymmetry surfaced by (b): the reservation is durable the moment it commits, so *any* writer death holding cache burns — terminate, OOM, or deploy.

---

## Q4: Ordering caveat — batched allocation vs code assuming seq order = commit order (observation only)

**Verdict:** under cached-block allocation, seq numbers detach from commit order and from xid-existence: a not-yet-visible seq's future writer may have **no xid at gap-detection time**, so the fence's permanence proof (`xmin >= fence_xmax`, subscriber_state.rs:350) can declare a gap permanent that is actually filled later from below — late materialization at a skipped seq becomes routine block-usage behavior, not an exceptional rollback artifact. Observations only; no fix designed.

**Method:** read the cited code at `bed756b` plus the insert/allocation path, tracing where seq values and xmins are read together. Files checked:

- `epoch_pg/src/event_bus/subscriber_state.rs` — `advance_contiguous_checkpoint`; fence proof `&& snap.xmin >= fence` at **350**, with its stated premise in the comment at 347–349: *"Every txn that was in-flight at detection has finished (xmin >= fence_xmax) and the sequence is still missing, so its writer aborted (or it was burned)."* Also the gap-model doc comments at 73 and 275.
- `epoch_pg/src/event_bus/mod.rs` — all bus fetches are seq cursors: `WHERE global_sequence > $1 ORDER BY global_sequence ASC LIMIT $2` at **856, 1263, 1973, 3547, 4077**; `visible_seqs`/`seq_to_id` built from those rows at **882–889 and 1996–2003**; `query_txid_snapshot` reads `pg_current_snapshot()` xmin/xmax at **201–216**; GapUnproven fired from the live-batch path at **544–556**; pinned-fence WARN (xmin < fence_xmax) at **607–621**; P4b self-heal excludes ReplayAlways at **1884–1893**; CLOUD-227 unbroken-prefix HWM guard at **3410–3424**.
- `epoch_pg/src/event_store.rs:145–153, 457–464` — `INSERT INTO ... (12 named columns, no global_sequence) ... RETURNING global_sequence`: today the seq is consumed by the *inserting transaction itself*, at INSERT time.
- `epoch_pg/src/migrations/m002_add_global_sequence.rs:31–42` — `CREATE SEQUENCE` + `global_sequence BIGINT DEFAULT nextval(...)`.
- `epoch_pg/src/migrations/m011_add_txid_to_events.rs` — per-row stamp: `txid BIGINT ... DEFAULT (pg_current_xact_id()::text::bigint)`, *"pg_current_xact_id() is volatile, so it is evaluated per row at INSERT time, stamping each new event with its inserting transaction's id"*; partial index on txid.
- `epoch_pg/src/migrations/m003_create_event_bus_infrastructure.rs:48,65,90` — NOTIFY payload carries `global_sequence`; checkpoints row `last_global_sequence BIGINT NOT NULL DEFAULT 0`; DLQ rows carry `global_sequence`.

**Evidence (the assumption, quoted):**

subscriber_state.rs:347–350:

```text
// FENCE (fast path): can we PROVE the gap is permanent? Every txn that
// was in-flight at detection has finished (xmin >= fence_xmax) and the
// sequence is still missing, so its writer aborted (or it was burned).
if let (Some(snap), Some(fence)) = (snapshot, fence_xmax)
    && snap.xmin >= fence
```

**What the code assumes, and what blocks change (traced, not runtime-reproduced):**

1. Today, seq consumption and xid acquisition are the same transaction: nextval fires inside the INSERT whose row is stamped with that txn's `txid` (m011). Therefore, when a gap at seq S is first observed, *if* a future writer of S exists, it already had an xid at allocation time — it is inside the `xmax` captured as the fence. Once `xmin >= fence_xmax`, that writer has finished; S still missing ⇒ it aborted. The fence proof is sound.
2. With cached blocks (option B), seq consumption happens in a *reservation* transaction that commits long before the row-inserting transactions exist. The future writer of a missing seq S can have no xid at detection time — or acquire one *after* the fence's `xmax` was captured. Such a writer is invisible to the fence; `xmin >= fence_xmax` can be satisfied while S's block holder is still alive and about to commit rows at S. A gap proven "permanent" can later be filled from below.
3. Concretely (issue's example, now with fence semantics): A reserves 1–64, B reserves 65–128; B commits 65..; the subscriber observes a gap at 1..64 with rows above, captures `fence_xmax`; A's insert txns start *after* detection, commit, and rows 1..64 appear below whatever the checkpoint/HWM did in the meantime. The CLOUD-227 unbroken-prefix guard (mod.rs:3410–3424) and the gap machinery already tolerate *transient* seq-vs-commit skew under nextval (NOTIFY fires at commit, so delivery-order skew exists today, m003:48 — bounded by one txn's duration); block reservation makes the skew *unbounded* (a writer may hold a cache arbitrarily long) and makes post-fence materialization *possible* rather than impossible.
4. Interaction with the current fail-closed ReplayAlways path: a block-shaped gap would wedge exactly as a burn-shaped gap does (backstop refuses after gap_timeout, GapUnproven at 544–556, HWM pinned by 3410–3424, no P4b self-heal per 1884–1893) — with the difference that the "burn" may still resolve itself if the holder eventually commits, or never resolve if it crashes (Q3b).

**Confidence:** verified for the code facts (all quotations above read this session at `bed756b`); the ordering consequence in points 1–4 is traced analysis, deliberately not runtime-reproduced, per the question's scope.

**Implications:** any CLOUD-261 design work on option B must treat "seq order = commit order" as an invariant the fence currently depends on and blocks would break; option A's "late-materializing row at a skipped seq → rebuild" detection becomes load-bearing under B even when no rollback occurred.

---

## Notes (adjacent observations, not chased)

- Benchmark harness blemish, disclosed: round 1's between-run `TRUNCATE` silently failed (POSIX-sh inline-env quirk in `run_bench.sh`), so round 1 measured runs had warmup rows present; round 2 ran with the reset applied. Neither affects the measured statement shapes; round 2's per-variant end-states were truncated by the loop, so round 1's end-state check is the quoted distinctness proof for v1/v2.
- The v3 invariant check doubles as an organic burn measurement: with block size 32 and 100-txn clients, 21.9% of allocated values were burned by block-tail waste alone (no crashes) — block size vs writer workload shape materially changes the burn rate.
- Scratch database `probe_cloud261_alloc` dropped; container-side scratch (`/tmp/b*.sql`, `/tmp/benchlog*`) removed; `git status --porcelain` clean apart from this report file.
