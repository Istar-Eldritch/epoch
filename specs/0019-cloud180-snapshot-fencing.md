# Spec 0019: `epoch_pg` Snapshot Fencing for Gap-Timeout Resolution

**Issue:** Linear CLOUD-180
**Status:** Proposed (ready for review)
**Created:** 2026-06-13
**Crate:** `epoch_pg`
**Commit scope:** `feat(pg)` / concept scope `event-bus`
**Builds on:** Spec 0016 (CLOUD-169 gap-timeout observability, shipped),
Spec 0002 (CLOUD reliability baseline)
**Depends on:** Spec 0018 (CLOUD-155) for migration numbering (see §3.1 and §11)
**PostgreSQL requirement:** PG 13+ only — pre-PG13 compatibility paths are not supported

---

## 1. Problem Statement

`PgEventBus` advances each subscriber's checkpoint across the global event
sequence. Because PostgreSQL sequences (`nextval`) are **non-transactional**, a
transaction can claim `global_sequence = N`, hold its INSERT uncommitted, while a
later transaction claims `N+1` and commits first. A reader then sees `N+1` but not
`N` — a **gap** at `N`.

The current resolver (`advance_contiguous_checkpoint` in
`epoch_pg/src/event_bus/subscriber_state.rs`) decides what to do with that gap
using a single heuristic: the **`gap_timeout`** (default 5 s, on
`ReliableDeliveryConfig`, `epoch_pg/src/event_bus/config.rs:198`). When a gap is
older than `gap_timeout`, it is **assumed** to belong to a rolled-back transaction
and the checkpoint advances past it.

This assumption is unsafe. If the transaction that holds `N` was merely **slow**
(large batch import, lock contention, a long-lived `AggregateTransaction` per spec
0007) and commits *after* the timeout fires, the committed event at `N` is
**silently and permanently skipped** for that subscriber — a data-loss mode.

CLOUD-169 (spec 0016, shipped) made this mode *observable*: every timeout-driven
skip now emits a `WARN`, writes a durable row to `epoch_event_bus_gap_timeouts`
(migration m009), and invokes an optional `on_gap_timeout` callback. CLOUD-169
explicitly **deferred** the mitigation and named its trigger condition (spec 0016
§8.3): "implement [snapshot fencing] as its own spec … for deployments relying on
long write transactions."

CLOUD-180 is that follow-up. It adds **snapshot fencing** (the Marten
high-water-mark pattern) so the gap resolver can *prove* whether a gap can still be
filled by an in-flight transaction, rather than guessing on a wall-clock timer.
`gap_timeout` is demoted from the primary decision to a bounded-staleness
**backstop**.

---

## 2. Goals and Non-Goals

### 2.1 Goals

- **G-1** Eliminate the silent-skip mode for the common case: a gap whose writer is
  still in-flight must be **held** (checkpoint does not advance) until the writer
  commits (gap fills) or aborts (gap proven permanent).
- **G-2** Resolve genuinely rolled-back / burned-sequence gaps **without** relying
  on the wall-clock `gap_timeout`, by proving — via PostgreSQL transaction
  visibility — that no in-flight transaction can ever fill the gap.
- **G-3** Keep `gap_timeout` as a **backstop** that still fires (and is still
  recorded via CLOUD-169 machinery) for pathological cases where the fence cannot
  clear: abandoned prepared transactions, `idle-in-transaction` sessions pinning
  `xmin`, or otherwise unbounded-open transactions.
- **G-4** Require PostgreSQL **13+** (`pg_current_xact_id` / `pg_current_snapshot`).
  No pre-13 compatibility code.
- **G-5** Degrade gracefully: if the `txid` column is unavailable (pre-migration
  database) or fencing is explicitly disabled, behaviour falls back to the existing
  timeout-only resolver with no panics. Snapshot query errors are also handled
  gracefully (warn + fallback for that batch).
- **G-6** No change to delivery semantics (still at-least-once) and no regression in
  in-order / catch-up paths.

### 2.2 Non-Goals

- **NG-1** Changing `epoch_core`, `epoch_mem`, or `epoch_derive`.
- **NG-2** Automatic replay of already-skipped events. The CLOUD-169
  `resolve_gap_timeout` operator workflow remains the recovery path for backstop
  skips.
- **NG-3** Retention/cleanup policy for `epoch_event_bus_gap_timeouts` (unchanged
  from spec 0016 §11).
- **NG-4** A built-in metrics dependency — counters remain the consumer's job via
  the `on_gap_timeout` callback.
- **NG-5** Backfilling `txid` for historical events. Pre-migration rows keep
  `NULL` `txid` and are treated as globally committed (§7.1).
- **NG-6** Horizontal scaling / outbox pattern (spec 0002 "Future Enhancements").
- **NG-7** Changing the `gap_timeout` default (stays 5 s) — though its *role*
  changes to a backstop.

---

## 3. Codebase Grounding (verified 2026-06-13)

| Fact | Location | Implication |
|------|----------|-------------|
| Two event INSERT paths: `store_events_in_tx` and `store_event`, both with an explicit column list | `event_store.rs:91-99`, `event_store.rs:348-355` | Populate `txid` via a **column `DEFAULT`** so neither INSERT statement (nor custom `events_table` routing) needs editing |
| `advance_contiguous_checkpoint(state, visible_seqs, gap_timeout) -> Vec<SkippedGap>` is pure, sync, DB-free; the timeout branch builds `SkippedGap`; `gap_first_seen: HashMap<u64, Instant>` records when each gap was first seen | `subscriber_state.rs:90-150` | The fence decision must stay pure. Pass the current snapshot bounds **in**; record the fence boundary in the gap tracker; keep all DB/log/callback side-effects in the caller |
| Sole production caller is `process_subscriber_for_batch`; it already owns `config`, `checkpoint_pool`, `subscriber_id`, emits the batched `WARN`, and fire-and-forgets the `epoch_event_bus_gap_timeouts` insert + `on_gap_timeout` callback | `mod.rs:62-260` | All new side-effects (record only *backstop* skips) live here; no new plumbing to reach a pool |
| Main listener loop builds `rows` then `visible_seqs: BTreeSet<u64>` from one batched catch-up query, wraps them in `Arc`, and passes a `BatchContext` to each concurrent subscriber task | `mod.rs:937-1045` | Query the current snapshot **once per batch** here and thread it through `BatchContext`; cheap (one round trip per poll) |
| `BatchContext { rows, visible_seqs, seq_to_id, config, dlq_pool, checkpoint_pool }` | `mod.rs:51-58` | Add `snapshot: Option<TxidSnapshot>` |
| `PgEventBus::new` / `with_table`; listener clones `self.pool` into `listener_pool`/`checkpoint_pool`/`dlq_pool` | `mod.rs:467-513, 688-690` | Detect server version once at listener start from `checkpoint_pool` |
| CLOUD-169 already shipped: `GapTimeoutInfo`, `GapTimeoutCallback`, `on_gap_timeout`, `SkippedGap`, `GapTimeoutEntry`, `list_gap_timeouts`, `resolve_gap_timeout`, migration m009 | `config.rs`, `subscriber_state.rs`, `mod.rs:340-1650`, `m009_create_gap_timeout_log.rs` | Reuse the recording/callback path verbatim for **backstop** skips only |
| Latest migration on `main` is **m009** (`create_gap_timeout_log`, version 9). Spec 0018 (CLOUD-155) reserves **m010** | `migrations/mod.rs:42,74`, `specs/0018-...md` | This work is **m011** assuming CLOUD-155 lands first (§3.1, §11) |
| `migrations_integration_tests.rs` asserts the count via **hard-coded `9`** in `test_migrator_runs_all_migrations`, `test_migrator_is_idempotent`, `test_migrator_pending_...`, `test_migrator_applied_...` (and per-version `applied_after[i]` name checks) | `epoch_pg/tests/migrations_integration_tests.rs` | These constants must be bumped (see §9 / Note); there is **no** `MIGRATION_COUNT` constant in the tree today |
| `ReliableDeliveryConfig` has a manual `Debug` and an **exhaustive** construction test `reliable_delivery_config_can_be_customized` (no `..Default::default()`) | `config.rs:255-275, 397-419` | Adding a config field requires updating `Default`, the `Debug` impl, and that test |
| Public re-exports of event-bus config/types | `lib.rs:60-63`, `event_bus/mod.rs:9-15` | Add any new public type to both lists |
| Integration harness: `#[serial]` + `#[tokio::test]`, fresh `Uuid::new_v4()` stream IDs, short `gap_timeout` via `ReliableDeliveryConfig` | `epoch_pg/tests/pgeventbus_integration_tests.rs` | New tests follow these conventions |

### 3.1 Migration numbering & dependency on CLOUD-155

`epoch_pg`'s migrator is **forward-only** and applies migrations strictly by
ascending `version()` (`migrations/mod.rs`). Spec 0018 (CLOUD-155) defines m010
(`strip_data_from_notify_payload`). This spec therefore claims **version 11**
(`m011_add_txid_to_events`). The `migrations_are_in_order` test only requires
strictly-increasing versions (gaps tolerated), so even if CLOUD-155 has not landed
the registry remains valid, but **CLOUD-180 should land after CLOUD-155**. If
CLOUD-180 ships first, renumber to m010 and let CLOUD-155 become m011 (see Open
Question OQ-1).

---

## 4. Design Overview

```
WRITER (any INSERT path)                 READER (per-batch, main listener loop)
┌──────────────────────────────┐         ┌────────────────────────────────────────────┐
│ INSERT INTO <events_table>    │         │ once/batch: query current txid snapshot      │
│  … (no txid in column list)   │         │   xmin = oldest in-flight, xmax = next-free  │
│ txid filled by column DEFAULT │         └───────────────────┬──────────────────────────┘
│  = pg_current_xact_id()       │                             │ BatchContext.snapshot
│    (PG13+)                    │                             ▼
└──────────────────────────────┘   advance_contiguous_checkpoint(state, visible, gap_timeout, snapshot)
                                    ┌─────────────────────────────────────────────────────┐
                                    │ gap at N (N+1 visible, N not visible):                │
                                    │  • first time → record (first_seen, fence_xmax=xmax)  │
                                    │  • else, FENCE: xmin ≥ fence_xmax ?                    │
                                    │      yes → every txn open at detection has finished   │
                                    │            and N still missing ⇒ PERMANENT gap        │
                                    │            → skip, reason = FenceCleared (no record)  │
                                    │      no  → an old txn is still in-flight; it may       │
                                    │            still commit N ⇒ HOLD checkpoint           │
                                    │  • BACKSTOP: held & first_seen.elapsed() > gap_timeout │
                                    │            → skip, reason = TimeoutBackstop            │
                                    └───────────────────────────┬─────────────────────────┘
                                                                │ Vec<SkippedGap{seq,dur,reason}>
                                                                ▼
                                    process_subscriber_for_batch (async, side-effects):
                                      FenceCleared  → debug log only (expected rollback)
                                      TimeoutBackstop → WARN + INSERT epoch_event_bus_gap_timeouts
                                                        + on_gap_timeout callback   (CLOUD-169)
```

### 4.1 Why the snapshot high-water mark is the correct fence

The crux: a gap at `N` means **no row for `N` is visible to the reader's
snapshot**. There is therefore *no `txid` to read for the missing sequence* — the
issue's phrasing "check whether the missing sequence's `txid` is still in-flight"
cannot be implemented literally (see OQ-2). The correct, provable fence uses the
session snapshot bounds:

- `xmin = pg_snapshot_xmin(pg_current_snapshot())` — the **oldest** transaction
  still in-flight. **Every transaction with `xid < xmin` has completed** (committed
  or aborted) and is no longer running anywhere.
- `xmax = pg_snapshot_xmax(pg_current_snapshot())` — the **first not-yet-assigned**
  transaction id. **Every transaction that has begun has `xid < xmax`.**

When a gap at `N` is **first observed**, the reader records `fence_xmax = xmax`
captured at that instant. The gap is provably **permanent** (safe to skip with no
data loss) once a later snapshot satisfies `xmin ≥ fence_xmax`, because:

1. The transaction that could fill `N` (if any) must have claimed `nextval = N`
   *before* the transaction that claimed the already-visible `N+1` claimed its
   value (`nextval` is monotonic). So that writer had **begun** by the time `N+1`
   was assigned, which is at-or-before our detection snapshot ⇒ its `xid <
   fence_xmax`.
2. `xmin ≥ fence_xmax` means **every** transaction with `xid < fence_xmax` has now
   completed. So `N`'s writer (if it existed) is done.
3. `N` is *still* not visible, therefore that writer **aborted** (rolled back), or
   `N` was a **burned** sequence value (cache loss / crash) and no writer ever held
   it. Either way `N` will never appear.
4. A **new** transaction beginning after detection cannot fill `N`: it will call
   `nextval` and obtain a value strictly greater than the current sequence maximum
   (`≥ N+1 > N`). It can never produce `N`.

Conversely, while `xmin < fence_xmax`, *some* transaction that was open at
detection time is still running and **might** be `N`'s writer about to commit —
so we **hold**. This is exactly G-1.

This argument needs **no per-event `txid`** for the hold/skip *decision*. The
`txid` column is still required for the reasons in §4.2.

### 4.2 Role of the `txid` column

The mandated `txid BIGINT` column on the events table earns its place by:

- **Diagnostics / forensics (primary):** when the backstop *does* fire, the
  recorded gap-timeout row can be correlated with the actual writing transaction
  after the fact. An operator investigating a backstop skip can query
  `epoch_events` for the row that *eventually* committed at the skipped sequence and
  read its `txid`, then cross-reference `pg_stat_activity` / logs. Without the
  column there is no way to attribute a late-committed event to its transaction.
- **Global-visibility classification:** lets a (future or operator) high-water query
  classify a *visible* event as "globally durable" (`txid < xmin`) vs "committed but
  still inside some snapshot's window". This underpins the CLOUD-169 detection query
  for *committed-but-skipped* events (spec 0016 §8.3) becoming precise.
- **Forward-compatibility:** a later refinement may use neighbour `txid`s to skip
  proven-aborted gaps even faster; recording it now avoids a second hot-table
  migration.

The hold/skip decision itself is made on the snapshot high-water mark (§4.1); the
`txid` column is **not** on the critical correctness path for that decision. This
divergence from the issue's literal wording is called out as OQ-2.

---

## 5. Detailed Design

### 5.1 Migration m011 — add `txid` to the events table

**New file:** `epoch_pg/src/migrations/m011_add_txid_to_events.rs`, implementing
`Migration` with `version() == 11`, `name() == "add_txid_to_events"`, registered as
the new last entry of `MIGRATIONS` in `migrations/mod.rs`.

Strategy — **nullable column + PG13+ `DEFAULT`**, no backfill:

1. `ALTER TABLE epoch_events ADD COLUMN IF NOT EXISTS txid BIGINT` — a nullable add
   with no default is a metadata-only operation (fast, no table rewrite). Existing
   rows keep `txid = NULL` (NG-5 / §7.1).
2. Set the column default for **future** inserts using `pg_current_xact_id()` (PG13+,
   returns `xid8`; cast to `bigint`):

```sql
ALTER TABLE epoch_events
    ALTER COLUMN txid SET DEFAULT (pg_current_xact_id()::text::bigint);
```

Notes:

- `pg_current_xact_id()` is **volatile**; as a column `DEFAULT`
  they are evaluated **per row at INSERT time**, stamping each new event with its
  inserting transaction's id. No INSERT statement changes are needed, so all paths
  (`store_event`, `store_events_in_tx`, and any custom `events_table`) get it for
  free **provided the custom table was created/migrated against `epoch_events`'s
  schema**.
- A partial index supports forensic lookups without bloating the hot path:
  ```sql
  CREATE INDEX IF NOT EXISTS idx_epoch_events_txid
      ON epoch_events (txid) WHERE txid IS NOT NULL;
  ```
- Forward-only (no `down()`), consistent with the migrator's design
  (`migrations/mod.rs:52-72`).

**Custom `events_table` auto-migration (OQ-5):** the migrator only manages
`epoch_events`, but `PgEventStore::with_table` and `PgEventBus` must automatically
ensure the `txid` column exists on any custom table at startup. This is handled by a
new `pub(crate)` helper:

```rust
/// Ensures the `txid` column exists on `table` with the correct DEFAULT.
/// Idempotent — safe to call on every startup. Uses `ADD COLUMN IF NOT EXISTS`
/// which is a metadata-only operation (no table rewrite) on PG13+.
pub(crate) async fn ensure_txid_column(pool: &PgPool, table: &str) -> Result<(), sqlx::Error>
```

The SQL it executes:

```sql
ALTER TABLE {table}
    ADD COLUMN IF NOT EXISTS txid BIGINT;
ALTER TABLE {table}
    ALTER COLUMN txid SET DEFAULT (pg_current_xact_id()::text::bigint);
CREATE INDEX IF NOT EXISTS idx_{table}_txid
    ON {table} (txid) WHERE txid IS NOT NULL;
```

All three statements are idempotent (`IF NOT EXISTS`). Called once during
`PgEventStore::with_table` construction (and the equivalent `PgEventBus` init path)
before the store/bus is returned to the caller. On error, log a `warn!` and
continue — fencing degrades to timeout-only for that table (§7.4), consistent with
G-5. **Not called for the default `epoch_events` table** (covered by m011).

### 5.2 Reader-side: snapshot acquisition

No server-version detection needed — PG13+ is the only supported target.
The snapshot SQL is fixed (see §5.3).

### 5.3 Reader-side: per-batch snapshot acquisition

`TxidSnapshot` — new lightweight value type (in `subscriber_state.rs`, made
`pub(crate)`):

```rust
/// A point-in-time view of PostgreSQL transaction-id boundaries used to fence
/// gap resolution. Captured once per catch-up batch from the reader's session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TxidSnapshot {
    /// Oldest still-in-flight xid. Every xid `< xmin` has completed.
    pub xmin: u64,
    /// First not-yet-assigned xid. Every begun transaction has xid `< xmax`.
    pub xmax: u64,
}
```

In the main loop, immediately after the catch-up `rows`/`visible_seqs` are built
(`mod.rs:965`), query the snapshot **conditionally** (OQ-4): skip the round trip
entirely when fencing is disabled (`snapshot_fencing = false`, §5.6) **or** when no
subscriber has active gaps (`gap_first_seen` is empty for all subscribers in this
batch). Query only when at least one subscriber has a tracked gap:

```sql
SELECT pg_snapshot_xmin(s)::text::bigint AS xmin,
       pg_snapshot_xmax(s)::text::bigint AS xmax
FROM pg_current_snapshot() s
```

A new gap observed for the first time in the current batch receives `fence_xmax =
None` (no snapshot yet); the snapshot is then queried on the **next** batch when
`gap_first_seen` is non-empty. This means the first hold of a brand-new gap is
always conservative (no fence-clear possible on first observation), which is correct.

Bind the result into `Option<TxidSnapshot>` and place it on `BatchContext`. On query
error: `log::warn!` once and pass `None` (graceful fallback to timeout-only, §7.4).

> **Connection-pool note.** The snapshot and all per-subscriber gap decisions for a
> batch are evaluated from the same logical poll. The fence only relies on `xmin`
> being *monotonic enough* across batches (it advances as transactions finish);
> taking the snapshot on whichever pooled connection serves the query is correct
> because `pg_snapshot_xmin` is a cluster-global quantity, not session-local state.

### 5.4 Reader-side: the pure fence in `advance_contiguous_checkpoint`

**Signature change** (ripples to all call sites + unit tests):

```rust
pub(crate) fn advance_contiguous_checkpoint(
    state: &mut SubscriberState,
    visible_seqs: &BTreeSet<u64>,
    gap_timeout: Duration,
    snapshot: Option<TxidSnapshot>,   // NEW; None = fencing unavailable/disabled
) -> Vec<SkippedGap>
```

**`SkippedGap` gains a reason discriminator:**

```rust
/// Why a gap was advanced past.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkipReason {
    /// The snapshot fence proved the gap is permanent: every transaction that was
    /// in-flight when the gap was first observed has completed and the sequence is
    /// still missing (writer aborted, or a burned sequence value). No data loss;
    /// NOT recorded as a gap-timeout.
    FenceCleared,
    /// The `gap_timeout` backstop fired while the fence still held (an old
    /// transaction is pinning `xmin`) or while fencing was unavailable. Potential
    /// data loss; recorded via CLOUD-169 machinery.
    TimeoutBackstop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkippedGap {
    pub skipped_sequence: u64,
    pub gap_duration: Duration,
    pub reason: SkipReason,   // NEW
}
```

**`SubscriberState.gap_first_seen` value type changes** from `Instant` to a small
struct carrying the fence boundary captured at first observation:

```rust
/// Per-gap tracking captured when a gap is first observed.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GapObservation {
    /// Monotonic instant the gap was first seen (drives the backstop timer).
    pub first_seen: Instant,
    /// `xmax` at first observation, or `None` if no snapshot was available then.
    /// The gap is permanent once a later `xmin >= fence_xmax`.
    pub fence_xmax: Option<u64>,
}
// gap_first_seen: HashMap<u64, GapObservation>
```

**Decision logic** for a confirmed gap at `next` (replacing the timeout-only branch
at `subscriber_state.rs:114-138`):

```text
entry = gap_first_seen.get(next)
if entry is None:
    // first observation: stamp time + fence boundary
    gap_first_seen.insert(next, GapObservation {
        first_seen: Instant::now(),
        fence_xmax: snapshot.map(|s| s.xmax),
    })
    break    // hold; re-evaluate next batch
else:
    // FENCE (fast path): can we PROVE the gap is permanent?
    if let (Some(snap), Some(fence_xmax)) = (snapshot, entry.fence_xmax):
        if snap.xmin >= fence_xmax:
            push SkippedGap { next, first_seen.elapsed(), FenceCleared }
            gap_first_seen.remove(next); contiguous_checkpoint = next; continue
    // BACKSTOP: fence not (yet) cleared, or fencing unavailable
    if entry.first_seen.elapsed() > gap_timeout:
        push SkippedGap { next, first_seen.elapsed(), TimeoutBackstop }
        gap_first_seen.remove(next); contiguous_checkpoint = next; continue
    break    // hold; in-flight writer may still commit
```

Properties:

- `snapshot == None` ⇒ `fence_xmax == None` ⇒ fence is skipped entirely ⇒ behaviour
  is **byte-for-byte the existing timeout-only resolver** (G-5). Existing unit tests
  pass a snapshot of `None`.
- A gap whose writer is in-flight keeps `xmin < fence_xmax` ⇒ held until commit
  (gap fills via `processed_ahead`, no `SkippedGap`) — G-1.
- A genuinely rolled-back gap clears the fence as soon as `xmin` advances past
  `fence_xmax` (typically well under `gap_timeout`) ⇒ `FenceCleared`, no record — G-2.
- A gap pinned by an unrelated long/abandoned transaction never clears the fence ⇒
  the backstop fires at `gap_timeout` ⇒ `TimeoutBackstop`, recorded — G-3.
- The function remains **pure, synchronous, DB-free** (NFR-3 of spec 0016 preserved).

### 5.5 Reader-side: side-effects in `process_subscriber_for_batch`

The caller (`mod.rs:157-260`) now branches on `SkipReason`:

- Partition the returned `Vec<SkippedGap>` into `FenceCleared` and `TimeoutBackstop`.
- **`FenceCleared`**: emit a single `log::debug!` summarising the proven-permanent
  sequences. **No** `WARN`, **no** `epoch_event_bus_gap_timeouts` insert, **no**
  callback (these are expected, lossless rollbacks).
- **`TimeoutBackstop`**: the batched CLOUD-169 `WARN` plus, if fencing was active and
  the fence did not clear (i.e. `fence_xmax` is `Some` and `xmin < fence_xmax` at
  backstop time), an **additional `WARN`** noting that the fence was persistently
  pinned — include the current `xmin` and `fence_xmax` values so operators can
  identify the offending long-running session via `pg_stat_activity` (OQ-3). Then
  fire-and-forget the `INSERT … ON CONFLICT DO NOTHING` + `on_gap_timeout` callback
  per gap, unchanged from the CLOUD-169 path.
- Wire the `BatchContext.snapshot` through into the `advance_contiguous_checkpoint`
  call at `mod.rs:157`.

### 5.6 Config

Add one field to `ReliableDeliveryConfig` (`config.rs`):

```rust
/// Whether to use PostgreSQL snapshot fencing (CLOUD-180) when resolving
/// sequence gaps.
///
/// When `true` (default), the gap resolver queries the current transaction
/// snapshot and only advances the checkpoint past a gap once it can prove no
/// in-flight transaction can still fill it. The `gap_timeout` then acts only as
/// a backstop for pathological cases (e.g. abandoned prepared transactions).
///
/// When `false`, the resolver reverts to the legacy `gap_timeout`-only
/// behaviour. Fencing also auto-disables for a batch if the snapshot query
/// fails or the `txid` primitives are unavailable.
///
/// Default: `true`
pub snapshot_fencing: bool,
```

Requirements (mirrors the CLOUD-169 field-addition checklist):

1. `Default` sets `snapshot_fencing: true`.
2. The manual `Debug` impl renders the field.
3. The exhaustive `reliable_delivery_config_can_be_customized` test gains the field.
4. Source-breaking only for field-by-field construction (consistent with prior
   additions `events_table`, `on_gap_timeout`); documented in CHANGELOG.

When `snapshot_fencing == false`, the main loop skips the per-batch snapshot query
and passes `None` into the resolver.

---

## 6. PostgreSQL Version Compatibility

PostgreSQL 13+ is the **minimum supported version**. All snapshot primitives use the
PG13+ API exclusively:

| Concern | API used | Notes |
|---------|----------|-------|
| Writer stamp | `pg_current_xact_id()::text::bigint` | Returns `xid8` (64-bit epoch-extended); cast to `bigint` |
| Reader snapshot bounds | `pg_snapshot_xmin/xmax(pg_current_snapshot())` | Returns 64-bit epoch-extended boundaries |
| Width | Both fit `BIGINT` / `u64` | Epoch-extended values do **not** wrap, so `xmin >= fence_xmax` comparisons are monotonic |

Both families return **epoch-extended** 64-bit ids (they never wrap inside a
cluster's lifetime), so the `u64` comparison in §5.4 is safe across the 32-bit xid
wraparound that affects raw `xid`. This is the explicit reason to record `BIGINT`,
not `INT`, and to use the `*xact_id` / `txid_*` families rather than raw `xid`.

---

## 7. Failure Modes and Edge Cases

### 7.1 NULL `txid` rows (pre-migration events)

Existing rows have `txid = NULL` (no backfill, §5.1). These are **always old,
committed** events. The reader never reads the missing sequence's `txid` (it can't,
§4.1), so NULL never participates in the hold/skip decision. A *visible* NULL-`txid`
row is treated as globally committed for any future high-water classification. No
special handling required in the resolver; the fence is driven purely by snapshot
bounds.

### 7.2 Prepared / abandoned / `idle-in-transaction` transactions

A `PREPARE TRANSACTION` left uncommitted, or a client stuck `idle-in-transaction`,
pins `xmin` below `fence_xmax` **indefinitely** ⇒ the fence never clears. This is
precisely the case the **backstop** exists for (G-3): after `gap_timeout` the gap is
skipped as `TimeoutBackstop` and recorded via CLOUD-169. Additionally, because the
fence was active but persistently pinned, a second `WARN` is emitted with the current
`xmin` and `fence_xmax` values (OQ-3 / §5.5) to help operators identify the offending
session via `pg_stat_activity` or `pg_prepared_xacts`.

### 7.3 In-flight snapshot races

- **Writer commits between fence check and skip:** if `xmin` had not yet passed
  `fence_xmax`, we *held* — we never skip a still-fenced gap, so the late commit is
  simply picked up on a subsequent batch (gap fills). No race window where a fenced
  gap is skipped.
- **`fence_xmax` captured with `None` then snapshot later available:** the gap was
  first seen while fencing was unavailable (`fence_xmax = None`), so it can only ever
  be resolved by the backstop, even if later batches have a snapshot. This is
  conservative (favours holding/timeout over an unproven fast-skip) and bounded by
  `gap_timeout`. A gap re-observed after a checkpoint reset starts a fresh
  observation with a current `fence_xmax`.
- **Sequence cache gaps (`CACHE > 1`, crash):** a sequence value may be *burned* —
  never assigned to any committed row and never to an in-flight one. The fence
  clears it as `FenceCleared` once `xmin >= fence_xmax` (no writer holds it). Correct
  and lossless.
- **Clock vs. xid:** the backstop uses the monotonic `tokio::time::Instant` already
  in `gap_first_seen` (no wall-clock); the fence uses xid arithmetic only. The two
  mechanisms are independent.

### 7.4 Graceful degradation

If the per-batch snapshot query errors (permissions, exotic PG fork, replica with
restricted functions), the reader logs `warn!` once for the batch and passes `None`
⇒ timeout-only behaviour for that batch. No panic, no checkpoint stall (G-5). If the
`txid` column is absent on a custom `events_table`, writes still succeed and fencing
no-ops for that bus.

### 7.5 Read replicas / hot standby

`pg_snapshot_xmin` on a standby reflects the standby's snapshot, which may lag the
primary. Since a subscriber's checkpoint is driven by what that reader can see, the
fence remains internally consistent with the reader's own visibility. (Subscribers
normally read the primary; documented as a note, not a code path.)

---

## 8. Functional Requirements

| ID | Requirement | Verify |
|----|-------------|--------|
| FR-1 | Migration m011 adds nullable `txid BIGINT` to `epoch_events` with `DEFAULT (pg_current_xact_id()::text::bigint)` and a partial index; forward-only; registered last in `MIGRATIONS` | Migration meta-tests + integration `setup()`; §10 `test_txid_column_exists_and_populates` |
| FR-2 | New inserts populate `txid` with the inserting transaction's id; existing rows stay `NULL` | §10 `test_txid_column_exists_and_populates` |
| FR-3 | `TxidSnapshot` and per-batch snapshot acquisition use PG13+ SQL and yield `Option<TxidSnapshot>` | Unit + integration (snapshot non-empty) |
| FR-4 | `advance_contiguous_checkpoint` takes `Option<TxidSnapshot>`; with `None` it is identical to today's resolver | Existing + new unit tests in `subscriber_state.rs` |
| FR-5 | A gap whose writer is in-flight is **held** (no skip, no record) until commit or abort | §10 `test_in_flight_transaction_gap_is_held` |
| FR-6 | A fence-cleared (proven-rolled-back) gap is skipped as `FenceCleared` with **no** `WARN`/record/callback | Unit + §10 `test_rolled_back_gap_fence_clears_without_record` |
| FR-7 | A gap pinned past `gap_timeout` is skipped as `TimeoutBackstop` and recorded via CLOUD-169 (`WARN` + row + callback) | §10 `test_pinned_gap_resolves_via_backstop` |
| FR-8 | `snapshot_fencing` config field defaults to `true`; `false` reverts to timeout-only; `Default`/`Debug`/exhaustive-test updated | `config.rs` unit tests |
| FR-9 | Snapshot query failure or missing primitives degrades to timeout-only without panic | Code review + unit (None path) |
| FR-10 | `ensure_txid_column` is called at `PgEventStore::with_table` and `PgEventBus` init for custom tables; failure degrades gracefully | Integration test with custom table name |
| FR-11 | Rustdoc on all new public/`pub(crate)` items; `cargo clippy --workspace -- -D warnings` and `cargo fmt --check` clean | CI |

---

## 9. Files to Modify

| File | Change |
|------|--------|
| `epoch_pg/src/migrations/m011_add_txid_to_events.rs` | **New** — `AddTxidToEvents` (version 11): nullable `txid` column, `pg_current_xact_id()` DEFAULT (PG13+), partial index |
| `epoch_pg/src/migrations/mod.rs` | Declare `mod m011_…`, `use AddTxidToEvents`, append `&AddTxidToEvents` to `MIGRATIONS` |
| `epoch_pg/src/event_store.rs` | Call `ensure_txid_column` during `PgEventStore::with_table` construction |
| `epoch_pg/src/event_bus/mod.rs` | Add `ensure_txid_column` helper; call it during `PgEventBus` init for custom tables; add `snapshot` to `BatchContext`; conditional per-batch snapshot query (PG13+); thread snapshot into `advance_contiguous_checkpoint`; partition `SkippedGap` by `SkipReason`; persistent-pin `WARN`; update backstop `WARN` wording |
| `epoch_pg/src/event_bus/subscriber_state.rs` | Add `TxidSnapshot`, `SkipReason`, `GapObservation`; change `SkippedGap` (add `reason`), `gap_first_seen` value type, and `advance_contiguous_checkpoint` signature + fence logic; extend unit tests |
| `epoch_pg/src/event_bus/config.rs` | Add `snapshot_fencing: bool`; update `Default`, `Debug`, exhaustive-construction test; add unit tests |
| `epoch_pg/tests/migrations_integration_tests.rs` | Bump hard-coded migration count `9 → 11` in the four count assertions; add `applied_after[9]`/`[10]` name/version checks (`strip_data_from_notify_payload` at 10, `add_txid_to_events` at 11); extend `teardown` to drop the `txid` index if needed |
| `epoch_pg/tests/pgeventbus_integration_tests.rs` | New integration tests (§10) |
| `CHANGELOG.md` | Unreleased entry: snapshot fencing (m011 `txid` column, reader fence, `snapshot_fencing` config, custom-table auto-migration, PG13+ requirement); note the exhaustive-construction source change |

No new dependencies (`sqlx`, `tokio`, `async_trait`, `chrono`, `uuid`, `log` already
used). No changes to `epoch_core`/`epoch_mem`/`epoch_derive`.

> **Note on migration count:** the task brief mentioned a `MIGRATION_COUNT`
> constant, but the tree at the time of writing hard-codes `9` in
> `migrations_integration_tests.rs`. This spec bumps the literals to `11`
> (assuming CLOUD-155's m010 is present). If a `MIGRATION_COUNT` constant has since
> been introduced, update that single constant instead. If CLOUD-155 is **not** yet
> merged when CLOUD-180 lands, the count becomes `10` and m011 must be renumbered to
> m010 (OQ-1).

---

## 10. Testing Strategy

### 10.1 Unit tests — `subscriber_state.rs`

| Test | Assertion |
|------|-----------|
| `fence_none_matches_legacy_timeout` (extend existing timeout tests with `snapshot = None`) | All existing timeout-only behaviours reproduce exactly; `SkipReason::TimeoutBackstop` |
| `fence_cleared_when_xmin_passed_fence_xmax` | First call (snapshot A) records `fence_xmax`; second call with `xmin >= fence_xmax` returns one `SkippedGap { reason: FenceCleared }` and advances |
| `fence_holds_when_xmin_below_fence_xmax` | With `xmin < fence_xmax` and elapsed `< gap_timeout`, returns empty `Vec` and checkpoint unchanged (held) |
| `fence_held_then_backstop_fires` | `xmin < fence_xmax` but elapsed `> gap_timeout` ⇒ `SkipReason::TimeoutBackstop` |
| `fence_xmax_none_only_resolves_via_backstop` | First observation with `snapshot = None` stamps `fence_xmax = None`; later `Some` snapshot does not fence-clear; only backstop resolves |
| `gap_fills_normally_no_skip` (extend) | Writer commits ⇒ sequence enters `processed_ahead` ⇒ no `SkippedGap` of any reason |

### 10.2 Unit tests — `config.rs`

| Test | Assertion |
|------|-----------|
| `snapshot_fencing_defaults_to_true` | `ReliableDeliveryConfig::default().snapshot_fencing` |
| `snapshot_fencing_can_be_disabled` | Field round-trips `false` |
| `config_debug_shows_snapshot_fencing` (extend `config_debug_output_shows_callback_presence`) | `Debug` renders the field |
| update `reliable_delivery_config_can_be_customized` | Exhaustive construction compiles with the new field |

### 10.3 Integration tests — `pgeventbus_integration_tests.rs`

All `#[serial]` + `#[tokio::test]`, fresh `Uuid::new_v4()` streams, assertions scoped
to the test's own subscriber id. These map directly to the issue's required tests.

| Test | Scenario | Maps to |
|------|----------|---------|
| `test_txid_column_exists_and_populates` | After migrations, insert an event and assert its `txid IS NOT NULL`; a row written before m011 (simulated via explicit `INSERT … (txid NULL)`) is tolerated by the reader | FR-1, FR-2 |
| `test_in_flight_transaction_gap_is_held` | Open tx A, INSERT event at the next sequence (claims `N`) but **do not commit**; in a separate committed tx insert the following event (`N+1`); subscribe with a generous `gap_timeout` (e.g. 30 s) and a short poll; assert within a few seconds that **no** `epoch_event_bus_gap_timeouts` row exists for this subscriber and the checkpoint has **not** advanced past `N`; then commit tx A and assert the event at `N` is delivered and the checkpoint advances | **Issue test (a)** / FR-5 |
| `test_rolled_back_gap_fence_clears_without_record` | Open tx A, INSERT to claim `N`, **ROLLBACK**; commit `N+1`; with no other long transactions open, assert the subscriber advances past `N` (delivers `N+1`) **without** any `gap_timeout` row (fence cleared, not backstop) | G-2 / FR-6 |
| `test_pinned_gap_resolves_via_backstop` | Open an **unrelated** sentinel tx (e.g. `SELECT txid_current()` then idle) to pin `xmin`; in another tx claim `N` then **ROLLBACK**; commit `N+1`; with a short `gap_timeout` assert a `epoch_event_bus_gap_timeouts` row eventually appears for the skipped `N` (backstop fired despite fencing) and `on_gap_timeout` fires once; then release the sentinel | **Issue test (b)** / G-3 / FR-7 |
| `test_fencing_disabled_uses_timeout` | `snapshot_fencing = false`, short `gap_timeout`: an in-flight gap is skipped via timeout and recorded (legacy behaviour) | FR-8 |
| `test_no_gap_timeout_record_on_in_order_events` (reuse from spec 0016) | In-order publishing produces zero gap rows of either reason | Regression |

> Sequence-hole construction reuses the spec 0016 harness technique (`setval` /
> explicit `global_sequence`), but CLOUD-180 prefers **real transactions** (open
> tx + uncommitted INSERT) so the `txid`/snapshot interaction is exercised
> end-to-end rather than simulated.

### 10.4 Regression

`cargo test --workspace` green; `cargo clippy --workspace -- -D warnings`;
`cargo fmt --check`. Migration meta-tests (`migrations_are_in_order`,
`all_migrations_have_unique_versions/names`) pass with m011.

---

## 11. Phased Delivery Plan

Phase 1 (migration) and Phase 2 (pure resolver) touch disjoint files and may proceed
in parallel. Phase 3 (reader wiring) depends on both. Phase 4 (config + integration +
acceptance) depends on Phase 3. Each phase follows TDD (red → green → refactor).

### Phase 1 — Migration m011 `txid` column + registry + migration tests

- **Goal:** Add the nullable `txid BIGINT` column with `pg_current_xact_id()` DEFAULT and
  partial index; register it; bump the migration-count assertions.
- **Create:** `epoch_pg/src/migrations/m011_add_txid_to_events.rs` (`AddTxidToEvents`,
  `version() -> 11`, `name() -> "add_txid_to_events"`, `up()` per §5.1).
- **Modify:** `migrations/mod.rs` (declare/import/append); `migrations_integration_tests.rs`
  (count `9 → 11`, add applied-name checks for versions 10 & 11); add
  `ensure_txid_column` helper to `event_bus/mod.rs` and wire into `PgEventStore::with_table`
  + `PgEventBus` init (`event_store.rs`, `event_bus/mod.rs`).
- **Out of bounds:** no fence/subscriber-state changes; no `down()`.
- **Exit:** `cargo test -p epoch_pg migrations::tests` and the migration integration
  suite pass; a fresh DB shows `txid` on `epoch_events`; a new insert populates it.
- **Effort:** S. **Difficulty:** standard. **Depends on:** CLOUD-155 m010 ideally
  present (OQ-1).

### Phase 2 — Pure snapshot fence in `subscriber_state.rs`

- **Goal:** Implement `TxidSnapshot`, `SkipReason`, `GapObservation`, the new
  `SkippedGap.reason`, the changed `gap_first_seen` value type, and the fence/backstop
  logic in `advance_contiguous_checkpoint` (§5.4), keeping the function pure.
- **Modify:** `subscriber_state.rs` (types, signature, logic, unit tests §10.1).
- **Out of bounds:** no DB/async/logging in this module; callers updated in Phase 3.
- **Exit:** §10.1 unit tests pass; `None` snapshot reproduces legacy behaviour
  exactly. (Crate will not fully compile until Phase 3 updates call sites — land the
  signature change behind Phase 3 or stub the caller in the same branch.)
- **Effort:** M. **Difficulty:** hard (proof-bearing logic; must preserve all
  existing gap invariants). **Depends on:** none.

### Phase 3 — Reader wiring in `mod.rs`

- **Goal:** Acquire the per-batch snapshot using PG13+ SQL (§5.3); add `snapshot`
  to `BatchContext`; thread it into `advance_contiguous_checkpoint`; partition results
  by `SkipReason` so only `TimeoutBackstop` is recorded; update the backstop `WARN`.
- **Modify:** `event_bus/mod.rs` (struct/loop/caller).
- **Out of bounds:** no change to the CLOUD-169 insert/callback SQL itself (reused);
  no change to catch-up buffer logic.
- **Exit:** crate compiles; `cargo test -p epoch_pg` (non-DB) green; manual/inline
  trace confirms `FenceCleared` is silent and `TimeoutBackstop` records.
- **Effort:** M. **Difficulty:** hard (snapshot acquisition + version branch + must
  not regress concurrency/checkpoint flow). **Depends on:** Phase 2 (and Phase 1 for
  the column at runtime).

### Phase 4 — Config flag, integration tests, lint, acceptance

- **Goal:** Add `snapshot_fencing` (§5.6); write the §10.3 integration tests; satisfy
  acceptance and lint gates.
- **Modify:** `config.rs` (field + tests); `pgeventbus_integration_tests.rs` (new
  tests); `CHANGELOG.md`.
- **Out of bounds:** no product-code changes beyond defects surfaced by tests.
- **Exit (acceptance):** issue tests (a) `test_in_flight_transaction_gap_is_held` and
  (b) `test_pinned_gap_resolves_via_backstop` pass; `test_rolled_back_gap_fence_clears_without_record`
  passes; `cargo clippy --workspace -- -D warnings` and `cargo test --workspace`
  green.
- **Effort:** M. **Difficulty:** hard (integration tests must orchestrate concurrent
  open transactions deterministically). **Depends on:** Phases 1–3; needs a running
  PostgreSQL.

### 11.1 Machine-readable phases

```json
{
  "issue": "CLOUD-180",
  "spec": "specs/0019-cloud180-snapshot-fencing.md",
  "depends_on_specs": ["specs/0018-cloud155-strip-data-from-notify-payload.md"],
  "phases": [
    {
      "phase": 1,
      "focus": "Migration m011 add txid column + registry + migration test count bump",
      "files": [
        "epoch_pg/src/migrations/m011_add_txid_to_events.rs",
        "epoch_pg/src/migrations/mod.rs",
        "epoch_pg/tests/migrations_integration_tests.rs"
      ],
      "effort": "S",
      "difficulty": "standard",
      "depends_on": [],
      "parallelizable_with": [2]
    },
    {
      "phase": 2,
      "focus": "Pure snapshot fence in subscriber_state (TxidSnapshot, SkipReason, GapObservation, fence/backstop logic)",
      "files": [
        "epoch_pg/src/event_bus/subscriber_state.rs"
      ],
      "effort": "M",
      "difficulty": "hard",
      "depends_on": [],
      "parallelizable_with": [1]
    },
    {
      "phase": 3,
      "focus": "Reader wiring: pg13 detection, per-batch snapshot query, BatchContext.snapshot, SkipReason partitioning",
      "files": [
        "epoch_pg/src/event_bus/mod.rs"
      ],
      "effort": "M",
      "difficulty": "hard",
      "depends_on": [1, 2]
    },
    {
      "phase": 4,
      "focus": "snapshot_fencing config flag, integration tests, CHANGELOG, lint/acceptance",
      "files": [
        "epoch_pg/src/event_bus/config.rs",
        "epoch_pg/tests/pgeventbus_integration_tests.rs",
        "CHANGELOG.md"
      ],
      "effort": "M",
      "difficulty": "hard",
      "depends_on": [3]
    }
  ]
}
```

---

## 12. Acceptance Criteria (mapped to CLOUD-180)

| ID | Criterion | Issue scope |
|----|-----------|-------------|
| AC-1 | `epoch_events` has a `txid BIGINT` column populated at insert via `pg_current_xact_id()` (PG13+), migration **m011** | Scope #1, #4 |
| AC-2 | The gap resolver queries the current snapshot and **holds** a gap whose writer is still in-flight (issue test (a)) | Scope #2, #5a |
| AC-3 | A genuinely rolled-back gap is resolved (fence-cleared fast, or via backstop when `xmin` is pinned), with the backstop still recording via CLOUD-169 (issue test (b)) | Scope #2, #3, #5b |
| AC-4 | `gap_timeout` no longer makes the primary decision; it fires only as a backstop and remains observable | Scope #3 |
| AC-5 | PG13+ only; disabled/unavailable fencing degrades to timeout-only without panic | Scope #4, G-5 |
| AC-6 | No regression: in-order/catch-up paths and existing CLOUD-169 records/APIs unchanged; clippy/fmt/tests green | Regression |

---

## 13. Open Questions

- **OQ-1 (migration numbering):** ~~Resolved~~ — CLOUD-155 (m010) merged to `main`
  on 2026-06-13 before this work began. This spec correctly claims **m011**.
- **OQ-2 (issue wording vs. mechanism):** ~~Resolved~~ — The snapshot high-water-mark
  fence is the accepted mechanism. The `txid` column is retained for diagnostics only
  (§4.2). No per-neighbour `txid` heuristic is needed.
- **OQ-3 (false-hold tolerance):** ~~Resolved~~ — Yes: emit a dedicated `WARN` when a
  gap's fence has been persistently pinned (i.e. `xmin` has not advanced past
  `fence_xmax` by the time the backstop fires). The message should include the current
  `xmin` and `fence_xmax` values so operators can identify the offending long-running
  session via `pg_stat_activity`. This fires alongside the existing `TimeoutBackstop`
  `WARN` (not instead of it).
- **OQ-4 (per-batch snapshot cost):** ~~Resolved~~ — Do not piggyback onto the
  catch-up query. Instead, query the snapshot **conditionally**: only when
  `gap_first_seen` is non-empty (active gaps exist) or a new gap was just detected in
  the current batch. In normal operation (no gaps) the round trip is skipped entirely.
  Implementation: check `!state.gap_first_seen.is_empty()` before the snapshot call;
  if a new gap appears mid-batch, it gets a `fence_xmax = None` observation and the
  snapshot is queried on the next batch.
- **OQ-5 (custom `events_table` migration):** ~~Resolved~~ — Auto-migrate. A
  `pub(crate) async fn ensure_txid_column(pool, table)` helper applies an idempotent
  `ADD COLUMN IF NOT EXISTS` + `SET DEFAULT` + `CREATE INDEX IF NOT EXISTS` at
  `PgEventStore::with_table` / `PgEventBus` startup (§5.1). On error: `warn!` and
  degrade to timeout-only. The default `epoch_events` table is covered by m011 and
  does not call this helper.
```
