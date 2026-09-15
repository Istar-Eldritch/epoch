# Spec 0030: Sequence-burn resilience for ReplayAlways subscribers

**Issue:** Linear [CLOUD-261](https://linear.app/catallactical/issue/CLOUD-261) — `nextval()` sequence burns wedge the sole fail-closed `ReplayAlways` subscriber. Ship a two-part fix: a consumer-driven opt-in gap policy (Part A, primary) and an opt-in burnless allocator that removes gaps at the source (Part B, secondary). · **Status:** Revised — Part A (primary) and Part B (opt-in `AllocationMode`, phased after A) both in scope; OQ-1/OQ-2/OQ-3 resolved (§6). · **Crate:** `epoch_core` (per-subscriber policy trait surface), `epoch_pg` (bus gap-resolver + late-materialization detection for A; counter-row allocator + migration for B); `epoch_mem` untouched (no sequence numbers). · **Scope:** `feat(core)`, `feat(pg)` — additive public API except one new `PgEventBusError` variant (a minor breaking change for downstream exhaustive matches, documented in CHANGELOG — R2/R9); Part A needs no migration, Part B adds one counter-row migration and a write-path change gated behind an opt-in mode (default `nextval`). · **Parent:** CLOUD-259. · **Sequel to:** specs 0026 / 0027 / 0028. · **Discovery / fuller history:** `specs/0030-cloud261-discovery.md` and the four verified probe reports under `docs/probes/cloud261-*-20260915.md`.

All line anchors below were personally read against source at `bed756b` this session; drift since then is possible and the implementation plan must re-anchor. Every "existing test pins X" claim was verified by opening the named test.

---

## 1. Problem

`epoch_events.global_sequence` is assigned by a column `DEFAULT nextval('epoch_events_global_sequence_seq')` (m002), evaluated per row inside the caller's insert transaction (`store_events_in_tx`, `epoch_pg/src/event_store.rs:144-152`; the column is omitted from the `INSERT`, `RETURNING global_sequence` reads it back). PostgreSQL sequences are non-transactional, so a rolled-back insert **permanently burns** every value it drew (alloc-cost probe Q1: a 10-row rollback burns 10 values; burn-wedge probe Q1: reserved seq consumed, row absent, next commit skips it). Under sustained load these burns are endemic — CLOUD-259 observed three in one acceptance session.

A burn below an in-flight bootstrap can deterministically wedge the sole fail-closed `ReplayAlways` subscriber:

1. The live batch observes the gap and arms the fence. If the fence stays unproven (`snap.xmin >= fence_xmax` never satisfied, `subscriber_state.rs:349-350`) past `gap_timeout` (default 5 s), the `FailClosed` arm of `advance_contiguous_checkpoint` refuses the backstop (`subscriber_state.rs:376-386`) and `GapUnproven` fires from the live-batch path — the only construction site in the crate (`mod.rs:544-556`).
2. The CLOUD-227 guard advances the `ReplayAlways` HWM only across an unbroken prefix (`advance_catchup_prefix`, `mod.rs:3416-3424`; live routing `mod.rs:708-714`), so the position pins below the hole.
3. P4b self-heal explicitly excludes `ReplayAlways` wedges (`mod.rs:1884-1893`: "the remedy is a fresh `subscribe()`"), so the subscriber is inert until something re-subscribes it.

### 1.1 The wedge is conditional (finding beyond the ticket)

With default `snapshot_fencing: true` and a plain `nextval` burn (no long-lived writer), there is **no wedge**: the fence proves the burn and the position crosses the hole in ~1.1 s via `FenceCleared`, zero halts (burn-wedge probe, Bonus). This matches the shipped suite test `test_fail_closed_gap_refusal_and_fence_cleared_recovery` (`epoch_pg/tests/pgeventbus_integration_tests.rs:8438`), whose doc pins exactly that recovery leg (verified by reading it this session).

The wedge reproduces only when the fence is **unavailable or unproven**: (a) `snapshot_fencing: false` — one deterministic burn wedges the subscriber (`GapUnproven` 5.05 s after the gap became visible, HWM pinned at 1 while head advanced to 6, all post-wedge events undelivered; burn-wedge probe Q2); or (b) an unrelated long-lived writer pinning `xmin` below `fence_xmax` past `gap_timeout` — the sustained-load case CLOUD-259 observed. Case (b) is precisely the in-flight-writer safety question any skip policy must be judged against.

### 1.2 Once wedged, nothing recovers a ReplayAlways subscriber (finding beyond the ticket)

- `GapUnproven` persists nothing: no halt flag on disk, no `epoch_event_bus_gap_timeouts` row (that table is written only for fail-open `TimeoutBackstop` skips, `mod.rs:636-702`), no DLQ row; the HWM is in-memory only (never persisted).
- `release_halt` (`mod.rs:2290`) writes the checkpoint row and fires `HaltReason::Released`, but a `ReplayAlways` subscriber never reads that row — it routes advancement to the in-memory HWM (`mod.rs:708-714`). So the release is a no-op on the live subscriber and its WARN "delivery resumes from N" **over-promises** (burn-wedge probe Q4b, verified against the code path this session).
- A fresh `subscribe()` resets the HWM to 0 (`mod.rs:3918-3920`), replays above the hole but **re-wedges at it**, and **double-delivers** the seqs immediately above the hole (delivery counts `(3,2)`, `(4,2)`; catch-up vs live-path dedup gap, burn-wedge probe Q4a).
- A row that commits **at** a skipped seq after the wedge is never detected: every bus fetch is `WHERE global_sequence > $1`, so nothing re-reads below a position (design-space probe Q4). The only below-observation in-tree is a documented operator SQL JOIN (`GapTimeoutEntry` doc, `mod.rs:958-965`).

### 1.3 The fix is a two-part story

The problem lives on two independent axes, and this spec now ships a deliverable on each. They are complementary, not competing, and are ordered by urgency:

- **Part A — `GapPolicy::SkipAfterBackstop` (primary, consumer-driven).** An opt-in, per-subscriber recovery contract for fold-style `ReplayAlways` projections: after the backstop would fire and the fence is still unproven, advance the HWM past the hole, record the skip in the existing audit ledger, and drive a rebuild when a late row materializes at a skipped seq. This is the concrete, consumer-driven win: catacloud carries ~530 lines of generation-chain halt-resilience (CLOUD-259 Phase 3, commit `614c06ae`) compensating for exactly this chain — review-verified and working, but integration-layer machinery standing in for a framework-level gap. Part A lets catacloud's policy-graph projection opt into a first-class recovery contract and retire that heal for the burn class. Part A is additive, needs no migration, and imposes **zero cost on the write path**. It is designed in §3.1–§3.5.

- **Part B — opt-in per-txn `AllocationMode` (secondary, removes gaps at the source).** For deployments that would rather never form the gap in the first place, an opt-in allocator draws each transaction's `global_sequence` values from a counter row inside the insert transaction, so a rollback or crash burns **zero** (alloc-cost probe Q1b/Q3a). It taxes every writer with counter-row serialization (3.5–4.4× slower at K=1; alloc-cost probe Q2), so it is opt-in with `nextval` remaining the default allocator; flipping the default later is a separate release decision made behind production data (§5.1). Part B is designed in §3.6.

Part A is the primary deliverable and keeps phase priority; Part B is phased after it (§9). The two are orthogonal: a deployment may run A alone, B alone (B removes the burn class A recovers from), or both (B for its own writers, A as belt-and-braces for any residual genuine hole).

---

## 2. What must NOT break

- **Fail-open and `Halt` stay the default on every path.** The new gap policy is opt-in per subscriber; a subscriber that does not request it is byte-for-byte unchanged.
- **The default allocator stays `nextval`, byte-for-byte.** Part B's `AllocationMode` defaults to `Nextval`; a deployment that does not opt in keeps the exact column-DEFAULT `nextval` write path (`INSERT` omits `global_sequence`, `RETURNING global_sequence` reads it back), the same burn semantics, and the same migrations. No write-path cost is imposed by the mere existence of the mode.
- **The two shipped fail-closed suite tests keep passing.** `test_fail_closed_gap_refusal_and_fence_cleared_recovery` (`:8438`) pins fence-cleared recovery + backstop refusal + `on_halt(GapUnproven)` fires once; `test_wedged_gap_does_not_starve_peer_and_release_resumes` (`:8731`) pins `Checkpointed` release + peer non-starvation (both read this session). This spec adds an opt-in mode; it must not alter the `Halt` semantics those tests pin.
- **`FenceCleared` still advances under both modes** (`subscriber_state.rs`): an event proven never to have existed cannot be missed, and the new policy does not touch that branch.
- **The 0026/0027 invariants hold — amended only where §3.7 says so.** Spec 0026 §3 proved the fence shows "writer finished, not aborted" — untouched. Spec 0027 §3.3 pinned "the persisted checkpoint MUST NEVER lead `state.contiguous_checkpoint`" — preserved untouched for every class: the opt-in class persists no checkpoint at all, and the only configuration that could violate the invariant (`SkipAfterBackstop` + `Checkpointed`) is rejected at registration (R2). What the new policy inverts is the fail-closed **hold** posture for the opt-in class's in-memory position; the one genuine amendment (to 0026 R5 readiness) is designed in §3.7.
- **`Event.global_sequence: Option<u64>` and its "monotonically increasing" contract survive** (`epoch_core/src/event.rs:66-73`) under either allocation mode; the counter-row mode preserves monotonicity and never changes the field.
- **The fence/snapshot machinery is unchanged by Part B.** Per-txn counter allocation is fence-safe by construction (§3.6, §5.2): the counter draw is inside the inserting transaction, so a missing seq's writer, if any, held an xid at detection — exactly the premise the fence already assumes under `nextval`. Part B does not touch `advance_contiguous_checkpoint`, the snapshot query, or any gap machinery.
- **`epoch_mem` is untouched** — it allocates no sequence numbers, so neither part imposes mem-backend parity work.

---

## 3. Design

### 3.1 Part A — `GapPolicy::SkipAfterBackstop` for opt-in ReplayAlways fold projections (primary)

A new per-subscriber policy, defaulting to today's behaviour:

```rust
// epoch_core/src/event_store.rs, alongside FailureMode (:289) and SubscriptionMode (:307)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum GapPolicy {
    /// Default. A fail-closed subscriber holds below an unproven gap (today's
    /// `GapUnproven` wedge). No behaviour change.
    #[default]
    Halt,
    /// Opt-in for fold-style `ReplayAlways` projections whose state is a pure
    /// function of currently-present rows. After the backstop would fire and the
    /// fence is still unproven, advance the HWM past the hole, record the skip,
    /// and rely on late-materialization detection (§3.3) to trigger a rebuild.
    SkipAfterBackstop,
}
```

Reachability follows the exact `failure_mode()` / `subscription_mode()` forwarding chain: a defaulted `gap_policy()` on `EventObserver`, `Projection` (`epoch_core/src/projection.rs:180-192` is where the two existing per-subscriber methods live), and `Saga`, with `ProjectionHandler`/`SagaHandler`/`SagaAdapter` forwarding the wrapped value. `subscribe()` resolves it once, in the same single-lock block that already resolves `failure_mode`/`subscription_mode`.

**Guardrail — `SkipAfterBackstop` is valid only for `ReplayAlways`.** For a `Checkpointed` subscriber, advancing past a hole would persist a checkpoint that leads the contiguous prefix, violating spec 0027 §3.3. `subscribe()` MUST reject `SkipAfterBackstop` + `Checkpointed` (a config error at registration, not a silent downgrade). This confines the 0026/0027 inversion to a subscriber whose position is ephemeral and unpersisted by construction. The guardrail is enforced at `epoch_pg`'s `subscribe()`; `epoch_mem` allocates no sequence numbers and imposes no gap policy, so it accepts the combination silently — a documented limitation, not a hazard (nothing there can wedge or skip).

**Hook.** `advance_contiguous_checkpoint` is the pure, DB-free resolver (`subscriber_state.rs:306-386`). Thread `gap_policy` in alongside `failure_mode`. In the `FailClosed` + `SkipAfterBackstop` case, the refused-backstop arm (`subscriber_state.rs:376-386`) instead takes the fail-open arm's advance: push `SkippedGap { reason: SkipReason::TimeoutBackstop, .. }`, remove from `gap_first_seen`, and advance `contiguous_checkpoint` — but only after the audit row is confirmed.

**Record-then-advance (review finding, round 1).** The existing recorder is fire-and-forget: the `INSERT INTO epoch_event_bus_gap_timeouts ... ON CONFLICT DO NOTHING` runs in `tokio::spawn` and only logs on failure (`mod.rs:642-690`, `error!` at `:658-663`), with the advance not gated on it (`mod.rs:694-714`). That is acceptable for the fail-open path (a missed row loses a log line) but not here: an unrecorded skip is a skip detection can never see (§3.3), which would silently void the §3.4 contract. For `SkipAfterBackstop` subscribers the caller (`mod.rs:558-702`) therefore confirms the ledger row synchronously **before** advancing the HWM; on write failure it does not advance — the subscriber stays in the refused-backstop posture and the skip is retried on a later tick. The rest of the machinery is unchanged: the batched WARN, the `on_gap_timeout` callback, and the `ReplayAlways` HWM routing (`mod.rs:708-714`); the only genuinely new state is the skipped-seq set (§3.3). T7 pins the failure path.

**Decision — the wedge path DOES start recording `epoch_event_bus_gap_timeouts` rows** for `SkipAfterBackstop` subscribers, precisely because the skip becomes a real `TimeoutBackstop` skip that flows through the existing recorder. This is the audited skipped-seq ledger Part A needs; it did not exist for the refused/wedged path before (§1.2). Its integrity is enforced by ordering, not by trust in the write: the HWM advances only once the row is confirmed (Hook above; T7). Fence-cleared skips under the new policy stay unrecorded, matching today's `FenceCleared` treatment (no data loss, nothing to detect).

### 3.2 Rebuild semantics

A `ReplayAlways` rebuild is, and stays, a fresh `subscribe()` from 0 with a fresh model (the mode's documented contract, `epoch_core/src/event_store.rs:317-325`). `SkipAfterBackstop` does not add a start-position parameter. The exposure window for a skipped-then-late row is therefore bounded by the lifetime of the in-memory model: the next rebuild replays from 0, the hole is filled by then, and the late row is picked up (failure-chain probe Note 4). The design's job is to *trigger* that rebuild promptly and audibly rather than leave the divergence silent.

### 3.3 Late-materialization detection (net-new machinery)

There is no automatic below-position observer today (design-space probe Q4). The minimal, already-shaped form: automate the documented JOIN. A periodic scan on the bus runs

```sql
SELECT g.* FROM epoch_event_bus_gap_timeouts g
JOIN {events_table} e ON e.global_sequence = g.skipped_sequence
WHERE g.bus_name = $1 AND g.resolved_at IS NULL
```

against unresolved rows for the bus's configured events table. The bus scoping is load-bearing (review finding, round 1): `bus_name` **is** the configured `events_table` (`mod.rs:631`, default `config.rs:393`, doc at `event_bus/config.rs:155-157`; `list_gap_timeouts` already filters `WHERE bus_name = $1`, `mod.rs:2932-2942`), and gap tests run on isolated tables — a literal join against `epoch_events` would find nothing (T4 could never pass) and a `with_table` deployment would silently never detect. Every ingredient exists — the table + `UNIQUE (bus_name, subscriber_id, skipped_sequence)` + `idx_epoch_gap_timeouts_sequence` + the unresolved partial index (m009 DDL, `m009_create_gap_timeout_log.rs:33-73`); `resolve_gap_timeout` is a `PgEventBus` method at `mod.rs:3046`. On a hit, fire a new **rebuild-needed signal** — a callback on the same `GapTimeoutInfo` callback surface as `on_gap_timeout` (`config.rs:163-241`) — carrying `(subscriber_id, skipped_sequence)`, **and mark that row resolved** (targeted `UPDATE ... SET resolved_at = now(), resolved_by = 'gap_detection'` keyed on the full `(bus_name, subscriber_id, skipped_sequence)` unique key — keying on the bus/seq pair alone would resolve other subscribers' rows and swallow their callbacks): one detection, one callback, per (subscriber, skipped seq). Without the resolution the scan would re-fire the callback every tick forever — the only `resolved_at` writer today is the operator call `resolve_gap_timeout(id, ...)`, `mod.rs:3046-3068`. The consumer's handler drops and re-`subscribe()`s a fresh model; the spec provides the trigger, not the rebuild (epoch cannot rebuild a consumer's model for it).

Cadence and whether the scan is a dedicated timer or folded into the existing listener tick is a plan-level tuning detail, not fixed here. The detection core itself is a public entry point (`check_skipped_gaps()`, name indicative) that runs the JOIN and fires the rebuild-needed callback per hit; the automatic scan is a configurable, disableable timer over that entry point (OQ-2 resolution), and its knob joins `ReliableDeliveryConfig` (`event_bus/config.rs:236-245`) — the "Field added | Default" compatibility table there must be updated with the new fields (the config is deliberately not `#[non_exhaustive]`; small in-tree churn, ~7 of 83 literals lack `..Default::default()`). Rustdoc must state the contract precisely for all three classes (review finding, round 1: fail-open subscribers DO write rows — the recorder loops over `timeout_backstop` skips irrespective of failure mode, `mod.rs:630-690`): for `SkipAfterBackstop` subscribers the entry point drives the detection-then-rebuild loop; for fail-open `TimeoutBackstop` `ReplayAlways` subscribers the same remedy applies — their position also advanced past a hole, so a detected late row fires the same rebuild-needed callback; for a default `Halt` subscriber it finds nothing (their wedges record no rows) and is a harmless read-only diagnostic — it is not a general recovery API.

### 3.4 The in-flight-writer safety argument (the reason `Halt` stays default)

A `SkipAfterBackstop` decision is made at backstop time, i.e. while the fence is by definition still pinned — exactly when an in-flight writer *may* still commit the skipped seq (failure-chain probe Note 1). The contract is safe **only** for the opt-in class: fold-style `ReplayAlways` projections whose state is a pure function of currently-present rows. For those, a transiently-missing row means a transiently-incomplete fold, detected by §3.3 and healed by a rebuild whose exposure is bounded by §3.2. This is why the policy is opt-in, `ReplayAlways`-only, and default-`Halt` — it is not a general-purpose "skip gaps" switch. The rustdoc on `GapPolicy::SkipAfterBackstop` must state this contract explicitly and reference the recorded-loss/rebuild-trigger pairing.

### 3.5 Adjacent defects (decision 4)

- **`release_halt` WARN over-promise for `ReplayAlways` — fix in scope.** The WARN "delivery resumes from N" (`mod.rs`, the block after `:2290`) is actively misleading for a `ReplayAlways` subscriber, which never reads the checkpoint row the call writes (verified: routing at `mod.rs:708-714`). This is a cheap, self-contained log/doc correction: either scope the WARN to `Checkpointed` or state plainly that a `ReplayAlways` subscriber is not resumed by release. Folded in because it is a one-line-ish honesty fix directly in this chain. There is no `ReplayAlways` recovery path added to `release_halt` (OQ-3): the remedy for a default-`Halt` wedge stays a fresh `subscribe()`, and once `SkipAfterBackstop` (or Part B) is in use the wedge no longer forms for opt-in subscribers.
- **Catch-up/live double-delivery after a fresh subscribe over a hole — ticket separately, out of scope.** The `(3,2)`/`(4,2)` double-delivery (burn-wedge probe Q4a; `processed_ahead` guards only the live path, not the catch-up pass) is a distinct correctness bug that affects *any* fresh subscribe over any hole, well beyond CLOUD-261. Both probes flagged it as worth its own investigation. Fixing it here would widen scope into the catch-up/live dedup boundary; it gets its own ticket.

### 3.6 Part B — opt-in per-txn `AllocationMode` (secondary; removes gaps at the source)

Part B gives a deployment an opt-in allocator that never burns on rollback or crash. It is bus/store-wide (a deployment-level choice, not per subscriber), selected on `PgEventStore` construction:

```rust
// epoch_pg (store/config surface), resolved once at PgEventStore construction
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum AllocationMode {
    /// Default. `global_sequence` is assigned by the column DEFAULT
    /// `nextval(...)`; the INSERT omits the column; a rolled-back or crashed
    /// txn permanently burns every value it drew. Byte-for-byte today's path.
    #[default]
    Nextval,
    /// Opt-in. Each insert transaction draws its `global_sequence` values from
    /// a counter row inside the same transaction, so a rollback rewinds the
    /// counter (zero burn) and a crash aborts it (zero burn). Serializes
    /// writers on the counter row (write-tax; §5.2); fence-safe by construction.
    PerTxnCounter,
}
```

**The migration (net-new; nothing reusable exists).** Design-space probe Q5 confirmed the schema contains no counter/lease/metadata row to reuse — the only allocator is the PG sequence itself. Part B adds a migration (next free number, m014 at time of writing) creating the counter **table** `epoch_events_sequence_counter(name TEXT PRIMARY KEY, val BIGINT NOT NULL)` — one row **per events table**, keyed by the events-table name (review finding, round 1: `PgEventStore.events_table` is configurable via `with_table`, `epoch_pg/src/event_store.rs:20, 73-93`, and every insert SQL is `format!`-ed with it, `:151, :462`; a single `'events'`-keyed row would collide across tables and leave the isolated-table tests T9-T15 unspecifiable). The migration creates only the table; per-table counter rows are created and seeded by the construction-time re-seed below (the migration cannot know custom table names). Seed floor: the sequence's last-**assigned** value read with `is_called` semantics — `SELECT last_value, is_called FROM <seq>` is the non-mutating read, and a virgin sequence (`is_called = false`) has assigned nothing — combined as `GREATEST(<last-assigned>, COALESCE(MAX(global_sequence), 0))`; the sequence name resolves via `pg_get_serial_sequence(events_table, 'global_sequence')`, the only generic route, which works because sequence ownership (`OWNED BY`) is what both the m002 default and the test helper (`{table}_seq` at tests:5734, ownership via `ALTER SEQUENCE ... OWNED BY` at :5752) establish. The migration always runs (the table exists regardless of mode); it is inert while the default `Nextval` mode is selected. This satisfies the migration-conventions house style: idempotent DDL, additive, no data rewrite of existing rows.

**Opt-in transition safety (review finding, round 1).** The migration alone is not sufficient for a safe opt-in: while the deployment still runs `Nextval`, writers keep advancing the sequence past any earlier seed, so opting in later could draw counter values that collide with sequence-assigned rows. The opt-in transition therefore requires (a) a quiesced writer set — the one-way operator step framed in OQ-4 — and (b) a construction-time re-seed: **every** construction under `PerTxnCounter` raises the per-table counter row to `GREATEST(existing counter, <sequence last-assigned, is_called-aware>, COALESCE(MAX(global_sequence), 0))` before serving writes, creating the row if absent — idempotent, and harmless on every restart once no `nextval` writers exist; the quiesce requirement attaches only to the one-way `Nextval`→`PerTxnCounter` transition. Because a silently-failed re-seed would allow collisions, the re-seed runs in a new **async, fallible constructor** (name indicative, e.g. `with_allocation_mode`; the existing `new`/`with_upcasters` are sync+infallible, `event_store.rs:34/:54`, and `with_table` swallows DB failures with `log::warn!`, `:73-92` — warn-and-continue is forbidden here): re-seed failure fails construction. `nextval` is non-transactional, so with writers quiesced the re-seed is collision-free by construction. T13 pins the observable (counter above high-water after construction, no allocator needed); T14 pins the end-to-end transition.

**The write-path change (isolated to the insert path).** Under `PerTxnCounter`, `store_events_in_tx` / `store_event` (`epoch_pg/src/event_store.rs:144-152`, `:457-462` — the entire allocation hook surface per design-space Q1) allocate `+K` **once** per transaction, where K is the number of events in that transaction (epoch's batch path already inserts K events in one txn), via `UPDATE epoch_events_sequence_counter SET val = val + K WHERE name = $events_table RETURNING val` (keyed by this store's events table) inside the caller's insert transaction, then supply the `global_sequence` values explicitly in the INSERT (the column is added to the column list; the DEFAULT does not fire). `RETURNING global_sequence` is preserved as the read-back contract (design-space Q2). Under the default `Nextval` mode the INSERT is untouched: column omitted, DEFAULT fires, no counter UPDATE. The per-txn (rather than per-row) allocation amortizes the counter cost across a multi-event aggregate transaction.

**Zero-burn semantics (measured).** Under `PerTxnCounter`, a rolled-back transaction rewinds the counter (the UPDATE is an uncommitted MVCC version, discarded) — **zero burn** (alloc-cost probe Q1b: counter reverts to 1, next committed writer gets 2, contiguous). A writer crash (`pg_terminate_backend` mid-txn) aborts the in-doubt transaction; the counter is unchanged and the next writer gets the same value — **zero burn** (alloc-cost probe Q3a). This is the entire point of the mode: it eliminates the burn class §1 is about, at the source, for opt-in deployments.

**Fence-safety (why no gap machinery changes).** The counter row lock serializes writers, so seq order equals commit order: a committed event above a hole can never appear, and a missing seq's writer, if any, held its xid inside the same transaction that drew the value — exactly the premise the fence already assumes under `nextval` (design-space Q5). So `PerTxnCounter` is fence-safe by construction and Part B touches none of `advance_contiguous_checkpoint`, the snapshot query, or the gap-timeout machinery. This is the decisive contrast with cached blocks, which break that premise and would drag fence rework into scope — rejected (§5.2). T15 pins the observable consequence (concurrent writers commit a gapless contiguous block, so gap machinery never engages). The rustdoc on `AllocationMode` must state this fence-safety contract and the write-tax trade-off (3.5–4.4× at K=1) explicitly, so the opt-in is an informed one.

### 3.7 Readiness under `SkipAfterBackstop` (the genuine 0026 R5 amendment)

Spec 0026 R5 pins: "Readiness MUST NOT report caught-up while a checkpoint is legitimately held below a hole" (`specs/0026-cloud226-contiguous-catchup-checkpoint.md:47`). `SkipAfterBackstop` changes exactly that observable for the opt-in class: the position advances past an unproven hole, so `wait_until_caught_up` **will** report caught-up across a possibly-still-arriving event. For this class that is the honest report — there is no persisted checkpoint held below the hole (the class persists none), and the residual divergence across the skipped seq is what §3.3's detection-then-rebuild loop exists to heal. 0027 §3.3 (persisted checkpoint never leads) is preserved untouched for every class (§2). Rustdoc on `GapPolicy::SkipAfterBackstop` and on the readiness method must state this amended contract; T8 pins it.

---

## 4. Requirements

Every requirement traces to the discovery doc or a probe report. R1–R9 are Part A; R10–R15 are Part B. NFRs are called out inline.

### Part A (primary)

- **R1** `GapPolicy` enum defined once in `epoch_core` with default `Halt`; defaulted `gap_policy()` on `EventObserver`/`Projection`/`Saga`; `ProjectionHandler`/`SagaHandler`/`SagaAdapter` forward the wrapped value (mirroring the `failure_mode()` chain, design-space Q2/Q3); existing implementors compile unchanged.
- **R2** `subscribe()` rejects `SkipAfterBackstop` + `Checkpointed` as a registration-time error via a new `PgEventBusError::InvalidSubscriptionConfig { subscriber_id, reason }` variant (the enum at `mod.rs:986` is not `#[non_exhaustive]`, so this is a minor breaking change for downstream exhaustive matches — documented in the header, R9, and CHANGELOG); accepts `SkipAfterBackstop` + `ReplayAlways` (guardrail, §3.1; enforced at `epoch_pg` only — `epoch_mem` imposes no gap policy).
- **R3** For a `SkipAfterBackstop` + `FailClosed` + `ReplayAlways` subscriber, an unproven gap past `gap_timeout` advances the HWM past the hole (no wedge) instead of firing `GapUnproven`, via the existing `TimeoutBackstop` path (failure-chain Q5; burn-wedge Q2).
- **R4** That skip records a **confirmed** `epoch_event_bus_gap_timeouts` row **before** the HWM advances (record-then-advance, §3.1) and fires `on_gap_timeout` (the wedge path becomes an audited skip); a ledger-write failure blocks the advance — the subscriber stays in the refused-backstop posture and retries (T7); `FenceCleared` skips remain unrecorded (design-space Q6; failure-chain Note 3).
- **R5** Late-materialization detection: a row committing at a skipped seq is detected by the bus-scoped JOIN (`WHERE g.bus_name = $events_table`, joined against that table — §3.3) over unresolved rows (automated periodic scan plus a public on-demand entry point sharing the same core) and fires a rebuild-needed callback for that subscriber, then marks that subscriber's row resolved so the callback fires once per (subscriber, skipped seq); the automatic scan is configurable and disableable, and its config fields update the `ReliableDeliveryConfig` compatibility table (design-space Q4; OQ-2).
- **R6** `Halt` (default) behaviour is byte-for-byte preserved on every path, including both shipped fail-closed suite tests (`:8438`, `:8731`). *(NFR: zero behavioural change for non-opting subscribers.)*
- **R7** No schema migration for Part A (reuses m009); `GapPolicy` and the rebuild callback carry rustdoc, including the §3.4 in-flight-writer safety contract.
- **R8** `release_halt`'s success WARN no longer claims delivery resumes for a `ReplayAlways` subscriber (burn-wedge Q4b; failure-chain Q4). No `ReplayAlways` recovery path is added to `release_halt` (OQ-3).
- **R9** Spec 0026 R5 (readiness) is explicitly amended for the `SkipAfterBackstop`+`ReplayAlways` opt-in class only: readiness may report caught-up across a skipped hole — the honest report for an unpersisted position (§3.7, T8). 0027 §3.3 (persisted checkpoint never leads) is **not** amended — it holds untouched for every class (guardrail R2; the opt-in class persists no checkpoint). Rustdoc and CHANGELOG state both; CHANGELOG also documents the new `PgEventBusError` variant (R2).

### Part B (opt-in, phased after A)

- **R10** `AllocationMode` enum (`Nextval` default | `PerTxnCounter` opt-in) selected once at `PgEventStore` construction; with `Nextval` the write path is byte-for-byte unchanged — INSERT omits `global_sequence`, column DEFAULT `nextval` fires, `RETURNING global_sequence` reads it back, no counter UPDATE (design-space Q1/Q2). *(NFR: default mode imposes zero write-path cost; the mode's mere existence taxes no writer.)*
- **R11** A migration adds the counter **table** keyed by events-table name (nothing reusable exists — design-space Q5); DDL idempotent and additive per migration conventions. Every construction under `PerTxnCounter` re-seeds the per-table counter row via a new async **fallible** constructor — `GREATEST(counter, sequence last-assigned (is_called-aware), MAX(global_sequence))`, row created if absent; failure fails construction (§3.6); the quiesced-writer requirement attaches to the one-way opt-in transition (OQ-4).
- **R12** Under `PerTxnCounter`, a multi-event transaction allocates `+K` once via `UPDATE ... SET val = val + K WHERE name = $events_table RETURNING val` (per-table key) inside the insert transaction and supplies the `global_sequence` values explicitly in the INSERT; `RETURNING global_sequence` is preserved (§3.6; design-space Q1/Q2; alloc-cost Q1b).
- **R13** Zero-burn semantics under `PerTxnCounter`: a rolled-back transaction burns zero (counter rewinds; alloc-cost Q1b) and a writer crash burns zero (MVCC abort; alloc-cost Q3a).
- **R14** The fence/snapshot machinery is valid under `PerTxnCounter` with no code change: the counter-row lock serializes writers so seq order equals commit order and the missing-seq writer holds an xid inside the inserting txn, preserving the fence premise (§3.6's serialization argument; design-space Q5 — the alloc-cost probe's Q4 caveat concerns the **rejected** cached-block option and motivates the contrast, not the claim). Pinned by T15 (concurrent writers commit a gapless contiguous block — no holes ⇒ gap machinery never engages). Part B modifies no gap machinery. *(NFR: no fence rework — the decisive reason cached blocks are rejected, §5.2.)*
- **R15** Rustdoc on `AllocationMode` states the fence-safety contract (§3.6/§5.2), the write-tax trade-off (3.5–4.4× at K=1; alloc-cost Q2), and the opt-in procedure (quiesced writers, construction-time re-seed, one-way per OQ-4); CHANGELOG notes the opt-in and that the default (`Nextval`) is unchanged.

---

## 5. Decisions (positions, not hedges)

The four decisions the discovery doc asked the spec to frame. Genuinely user-level choices are lifted to §6 Open Questions with a recommendation attached.

### 5.1 Decision 1 — option A, B, or both

**Position: both, phased — ship A as the primary fix, and Part B (per-txn) as an opt-in allocator with `nextval` remaining the default; reject cached blocks.** The two options sit at different layers and are complementary, not competing — and they are now both in scope, ordered by urgency.

- The concrete, consumer-driven defect is that a wedged `ReplayAlways` fold projection has **no** recovery path (§1.2): fresh subscribe re-wedges and double-delivers, `release_halt` is a silent no-op. Part A gives that class a first-class, opt-in, self-auditing recovery and lets catacloud retire its heal. It is additive, needs no migration, imposes **zero cost on the write path**, and therefore keeps phase priority.
- Part B (per-txn) eliminates the gap at the source but taxes **every** writer with counter-row serialization (3.5–4.4× slower at K=1, worsening with concurrency; alloc-cost probe Q2). Imposing that *by default* on a write-hot event-store framework, to fix a *conditional* wedge (§1.1), is the wrong altitude. So B ships as an **opt-in `AllocationMode`** with `nextval` as the default: the write-tax argument keeps the default unchanged, and flipping the default later is a separate release decision to be made behind production data. This is the OQ-1 resolution folded into the spec.

So: A ships first (primary); B ships as the opt-in secondary lever, phased after A (§9).

### 5.2 Decision 2 — per-txn exact vs cached blocks

**Position: per-txn exact allocation; cached blocks rejected.**

- **Per-txn exact** (`UPDATE counter SET val = val + K RETURNING val` inside the insert txn) burns **zero** on rollback (counter rewinds; alloc-cost probe Q1b) and zero on crash (MVCC abort; Q3a). Critically it is *fence-safe*: the counter row lock serializes writers, so seq order equals commit order and a committed event above a hole can never appear — the "future filler has no xid" hazard cannot arise, so the fence machinery needs no change (R14). Cost is the write serialization (3.5–4.4× at K=1), amortized by allocating `+K` **once** per multi-event aggregate transaction rather than per row (`store_events_in_tx`, `epoch_pg/src/event_store.rs:144-152`).
- **Cached blocks** are fast on the hot path but break the fence premise: a block reserved and committed ahead of time means the future writer of a missing seq may hold no xid at gap-detection, so a fence-"proven" gap can materialize rows from below (alloc-cost probe Q4; discovery "Critical hazard"). That drags fence-semantics rework into scope — a much larger, riskier change than the allocator itself. Rejected, and kept out of scope (§8).

### 5.3 Decision 3 — Part A policy shape

Settled above: a per-subscriber `GapPolicy` enum (`Halt` default | `SkipAfterBackstop` opt-in), `ReplayAlways`-only (R2), threaded into the pure resolver (§3.1); skipped-seq tracking via the existing `epoch_event_bus_gap_timeouts` ledger (the wedge path **does** start recording, R4); late-row detection via the automated JOIN + a public on-demand entry point + a rebuild-needed callback (§3.3, OQ-2); rebuild semantics = fresh `subscribe()` from 0 with bounded exposure (§3.2).

### 5.4 Decision 4 — adjacent defects

Settled in §3.5: fix the `release_halt` WARN over-promise in scope (R8), with no new `ReplayAlways` recovery path in `release_halt` (OQ-3); ticket the fresh-subscribe double-delivery separately (out of scope).

---

## 6. Open Questions

All three original open questions are resolved records. One new Part-B integration question (OQ-4) is genuinely user-level and carries a recommendation; the rest of Part B's mechanics are settled from probe evidence and need no user decision.

- **OQ-1 — RESOLVED (2026-09-15, interview): Part B is folded into this spec.** Per-txn exact allocation only (§5.2 position stands; cached blocks stay rejected), shipped as an opt-in `AllocationMode` with `nextval` remaining the default allocator (§5.1 write-tax argument keeps the default unchanged; flipping it later is a separate release decision behind production data). This spec grows the counter-row migration (R11), the write-path mode (R10/R12), zero-burn semantics (R13), fence-validity (R14), and rustdoc/CHANGELOG (R15). Part A remains the primary, consumer-driven fix and keeps phase priority. §1.3, §3.6, §5.1, §5.2, §7, §8, the Requirements, and the Delivery Plan carry the resolution.
- **OQ-2 — RESOLVED (2026-09-15, interview):** automated periodic scan as primary, behind the `SkipAfterBackstop` opt-in only; detection core exposed as a public on-demand entry point; scan configurable and disableable. Rationale: detection is the second half of the §3.4 safety contract, not optional plumbing; the both-shape costs near zero because the scan is a timer over the same entry point, and it buys cadence-independent tests plus on-demand/operator triggering. §3.3, R5, and T4 carry the resolution.
- **OQ-3 — RESOLVED (2026-09-15, interview): no real `ReplayAlways` recovery path in `release_halt`.** R8 stays the WARN-honesty fix only; a fresh `subscribe()` remains the remedy for a default-`Halt` ReplayAlways wedge, and once `SkipAfterBackstop` (and Part B) ship, the wedge it would release no longer forms for opt-in subscribers. The catch-up/live double-delivery bug remains ticketed separately, out of scope (§3.5, §8).
- **OQ-4 — RESOLVED (2026-09-15, interview): `AllocationMode` is a one-way, deployment-lifetime choice for this release.** Opting in is an operator action (quiesced writers + construction-time re-seed, §3.6/R11); switching back is not a built path — it remains a documented operator procedure (quiesce, `setval` the sequence past the counter's high-water, restart under `Nextval`). Rationale: one-way eliminates the mixed-allocator concurrency hazard by construction, keeps Part B's surface small, and matches the "flip the default later behind production data" framing (§5.1); a runtime-switchable knob can be its own ticket if a deployment ever needs it (§8).

---

## 7. Test plan (paper)

Discipline per 0026/0027/0028: relative sequence values via `INSERT ... RETURNING global_sequence`, `isolated_events_table` for gap tests (own sequence, no shared-table burn), no absolute sequence assertions, DB-gated (`EPOCH_REQUIRE_DB=1`) + `#[serial]`. Part B tests reuse the same isolation: an isolated events table plus its own counter row, relative-value assertions only.

### Part A

| # | Test | Setup sketch | Key assertions |
|---|---|---|---|
| T0 | Forwarding-chain reachability (R1) | `Projection` overriding `gap_policy() -> SkipAfterBackstop`, wrapped in `ProjectionHandler` (epoch_core unit test) | `EventObserver::gap_policy()` on the handler returns `SkipAfterBackstop` — override reaches through the handler |
| T1 | Config guardrail (R2) | subscribe `SkipAfterBackstop` + `Checkpointed`; and `SkipAfterBackstop` + `ReplayAlways` | first is a registration `Err`; second `Ok` |
| T2 | Skip-after-backstop instead of wedge (R3) | isolated table, `snapshot_fencing: false`, `gap_timeout: 500ms`, `ReplayAlways`+`FailClosed`+`SkipAfterBackstop`; `isolated_events_table` does NOT copy triggers, so run `setup_trigger()` with a `Uuid`-unique channel before `start_listener()` (tests:5727-5730); burn a hole, commit events above it | HWM advances past the hole; no `GapUnproven`; post-hole events delivered |
| T3 | Skip is audited, record-before-advance (R4) | T2 continued | one `epoch_event_bus_gap_timeouts` row for the skipped seq, present **before** the HWM advance (ordering asserted); `on_gap_timeout` fired once |
| T4 | Late-materialization triggers rebuild, once (R5) | T3, then insert a row at the skipped seq | detection core (invoked directly, for cadence-independence) detects it; rebuild-needed callback fires with the skipped seq; that subscriber's row is marked resolved (a second scan fires nothing); auto-scan fires it too when enabled |
| T5 | `Halt` default unchanged (R6) | the two shipped fail-closed tests `:8438`, `:8731` run unchanged | both green; `GapUnproven` still fires for a default subscriber |
| T6 | `release_halt` WARN honesty (R8) | wedge a `ReplayAlways` subscriber, call `release_halt` | WARN does not claim delivery resumes for a `ReplayAlways` subscriber |
| T7 | Ledger-write failure blocks the skip (R4) | T2 setup with an injected ledger INSERT failure | no advance, no skip: the subscriber stays in the refused-backstop posture; HWM unchanged; the skip lands on a later tick once the write succeeds (record-then-advance, §3.1) |
| T8 | Readiness across a skipped hole (R9) | T3 continued | `wait_until_caught_up` reports caught-up for the opt-in subscriber — the amended 0026 R5 contract (§3.7) |

### Part B

| # | Test | Setup sketch | Key assertions |
|---|---|---|---|
| T9 | Zero-burn on rollback under `PerTxnCounter` (R13) | isolated table + its own counter row, `AllocationMode::PerTxnCounter`; commit an anchor, `BEGIN` a K-row insert (counter draws +K), `ROLLBACK`, then a committed writer | counter rewinds; next committed writer's `global_sequence` is exactly `anchor + 1` (contiguous, relative) — no burn |
| T10 | Zero-burn on crash under `PerTxnCounter` (R13) | isolated table + counter; open a txn on a **dedicated `PgConnection`** that draws +K but never commits; terminate it via `pg_backend_pid()` + `pg_terminate_backend` (net-new test infrastructure — no in-tree precedent); then a fresh committed writer | aborted txn leaves the counter unchanged; the fresh writer gets the same relative value the aborted txn drew — no committed value reused |
| T11 | Default-mode regression + opt-in isolation (R10) — **P7's merge gate** | `AllocationMode::Nextval` (default): rollback a K-row insert, then commit | write path unchanged — the rolled-back txn burns K (relative gap of K), matching today; no counter row is touched; existing insert tests green |
| T12 | Allocate +K once per multi-event txn (R12) | `PerTxnCounter`; one aggregate transaction storing K events via `store_events_in_tx` | the K events receive a contiguous `global_sequence` range drawn in a single counter step; `RETURNING global_sequence` yields all K values |
| T13 | Opt-in re-seed observable (R11) | isolated table + counter table; under `Nextval`, advance the sequence past any earlier seed; quiesce; construct with `PerTxnCounter` | after construction the per-table counter row's `val` exceeds the sequence high-water — asserted via direct SQL on the counter row, no allocator needed |
| T14 | Opt-in collision-freedom end-to-end (R11/R13) | T13 continued; commit a writer | the first counter-assigned seq exceeds every previously assigned value — no collision |
| T15 | Concurrent-writer contiguity (R14) | `PerTxnCounter`; N concurrent `store_event` writers | committed seqs form a gapless contiguous block — no holes, so gap machinery never engages (fence premise holds, §3.6) |

Traceability (per 0028's house pattern, `specs/0028-cloud216-fail-closed-subscriber-semantics.md:443`): R1→T0, R2→T1, R3→T2, R4→T3+T7, R5→T4, R6→T5, R8→T6, R9→T8, R10→T11, R11→T13+T14, R12→T12, R13→T9+T10, R14→T15. **R7 and R15 are doc-only** (rustdoc/CHANGELOG obligations, verified by review in P5/P8); every other requirement traces to a numbered test.

---

## 8. Out of scope

- Cached-block allocation — rejected (§5.2): it breaks the fence premise and would drag fence-semantics rework into scope.
- The catch-up/live double-delivery bug after a fresh subscribe over a hole — its own ticket (§3.5).
- A live switch-back path from `PerTxnCounter` to `Nextval` — resolved as a one-way deployment-lifetime choice for this release (OQ-4, resolved); a switchable knob is its own ticket if ever needed.
- `SkipAfterBackstop` for `Checkpointed` subscribers — forbidden by construction (§3.1, R2).
- Catacloud-side heal removal — tracked in catacloud; this spec provides the contract it consumes.

---

## 9. Delivery Plan (TDD; each phase: failing test → implement → refactor)

Part A ships first and keeps priority (P1–P5). Part B's phases (P6–P8) come after, and depend on **each other** (migration → write-path → tests/docs), not on Part A's later phases.

**Parallelism (chosen, not hedged):** Part B touches a disjoint file surface from Part A's later phases — Part B works in `epoch_pg/src/migrations/` and `epoch_pg/src/event_store.rs` (the insert path), while Part A's P4/P5 work in `epoch_pg/src/event_bus/`. So P6 (migration) and P7 (allocator + write-path) **may run in parallel with A's P4/P5 if staffing allows**, and P8 (tests/docs) closes B after P7. The one caution: the write-path change in P7 is the riskiest edit in either part (it moves allocation off the column DEFAULT), so it must land behind the `PerTxnCounter` gate with the default `Nextval` regression pin (T11) green before it merges — interleaving is fine, weakening that gate is not. If staffing is thin, run P6→P7→P8 strictly after P5; the dependency order within B is fixed either way.

- **P1 — `GapPolicy` trait surface (epoch_core).** T0 red → green. Add `GapPolicy` beside `FailureMode`/`SubscriptionMode`; defaulted `gap_policy()` on `EventObserver`/`Projection`/`Saga` plus the handler/adapter forwarding chain. (R1)
- **P2 — resolve + guardrail (epoch_pg subscribe).** T1 red → green. Resolve `gap_policy` in the `subscribe()` single-lock block; reject `SkipAfterBackstop` + `Checkpointed`; carry `gap_policy` onto `SubscriberState`. (R2)
- **P3 — `SkipAfterBackstop` in the pure resolver + audited skip.** T2, T3, T7 red → green. Thread `gap_policy` into `advance_contiguous_checkpoint`; `FailClosed`+`SkipAfterBackstop` takes the `TimeoutBackstop` advance path (`subscriber_state.rs:376-386`) **after the confirmed ledger insert** (record-then-advance, §3.1); the ledger INSERT is made synchronous-confirmed for this path while the fail-open path keeps its fire-and-forget recorder; the existing WARN + `on_gap_timeout` + HWM routing (`mod.rs:558-714`) run unchanged. (R3, R4)
- **P4 — late-materialization detection + rebuild callback.** T4 red → green. Automate the `gap_timeouts ⋈ {events_table}` JOIN (`WHERE g.bus_name = $1`) over unresolved rows for the bus's table; expose the detection core as a public on-demand entry point; fire a rebuild-needed callback on the GapTimeout callback surface (`config.rs:163-241`) and mark each hit row resolved; make the auto-scan configurable/disableable and update the `ReliableDeliveryConfig` compatibility table. (R5)
- **P5 — `release_halt` WARN honesty + regression pins + docs.** T5, T6, T8 red → green. Scope the `release_halt` success WARN so it does not over-promise for `ReplayAlways` (block after `:2290`); rustdoc on `GapPolicy` incl. the §3.4 safety contract and the §3.7 readiness amendment; CHANGELOG amending 0026 R5, clarifying (not amending) 0027 §3.3, and documenting the new `PgEventBusError::InvalidSubscriptionConfig` variant; `cargo fmt` + `clippy -D warnings`; full DB-gated suite. (R6, R7, R8, R9)
- **P6 — counter-row migration + `AllocationMode` surface (Part B).** T13 red → green (counter observable via direct SQL; no allocator needed). Add the m014 migration creating the per-table-keyed counter table; add the `AllocationMode` enum (default `Nextval`) and the new async **fallible** constructor (name indicative, e.g. `with_allocation_mode`) that creates/seeds the per-table counter row (is_called-aware seed floor, §3.6) and fails construction on re-seed failure; the default path stays byte-for-byte unchanged. (No store-level config module exists; the spec's `config.rs` anchors are `epoch_pg/src/event_bus/config.rs`.) (R10, R11)
- **P7 — `PerTxnCounter` write-path (Part B).** T9, T10, T11, T12, T14, T15 red → green. Under `PerTxnCounter`, `store_events_in_tx`/`store_event` allocate `+K` once via the per-table-keyed counter UPDATE inside the insert txn and supply `global_sequence` explicitly; `RETURNING global_sequence` preserved; zero-burn on rollback and crash; no gap-machinery change (fence-safe by construction, pinned by T15). **T11 (default `Nextval` regression pin) is this phase's merge gate.** (R12, R13, R14)
- **P8 — Part B docs + consolidation.** No new tests; keep T9–T15 green. Rustdoc on `AllocationMode` stating the fence-safety contract and the write-tax trade-off; CHANGELOG noting the opt-in and unchanged default; `cargo fmt` + `clippy -D warnings`; full DB-gated suite. (R15)

---

## Phases (JSON)

```json
{
  "phases": [
    {
      "phase": 1,
      "focus": "GapPolicy trait surface in epoch_core",
      "effort": "S",
      "difficulty": "standard",
      "id": "P1",
      "name": "GapPolicy trait surface (epoch_core)",
      "tdd": "T0 red \u2192 green",
      "scope": [
        "epoch_core/src/event_store.rs: add GapPolicy enum beside FailureMode (:289) / SubscriptionMode (:307), default Halt",
        "epoch_core/src/projection.rs: defaulted gap_policy() beside subscription_mode()/failure_mode() (:180-192); Saga + handler/adapter forwarding chain"
      ],
      "requirements": [
        "R1"
      ],
      "tests": [
        "T0"
      ]
    },
    {
      "phase": 2,
      "focus": "Resolve gap_policy and guardrail at subscribe",
      "effort": "S",
      "difficulty": "standard",
      "id": "P2",
      "name": "Resolve + guardrail (epoch_pg subscribe)",
      "tdd": "T1 red \u2192 green",
      "scope": [
        "epoch_pg/src/event_bus/mod.rs: resolve gap_policy in the subscribe() single-lock block; reject SkipAfterBackstop + Checkpointed at registration via a new PgEventBusError::InvalidSubscriptionConfig variant (minor breaking for exhaustive matches; documented); carry gap_policy onto SubscriberState"
      ],
      "requirements": [
        "R2"
      ],
      "tests": [
        "T1"
      ]
    },
    {
      "phase": 3,
      "focus": "SkipAfterBackstop resolver arm + audited skip",
      "effort": "M",
      "difficulty": "hard",
      "id": "P3",
      "name": "SkipAfterBackstop in the pure resolver + audited skip",
      "tdd": "T2, T3, T7 red \u2192 green",
      "scope": [
        "epoch_pg/src/event_bus/subscriber_state.rs: thread gap_policy into advance_contiguous_checkpoint; FailClosed+SkipAfterBackstop takes the TimeoutBackstop advance path (:376-386) after the confirmed ledger insert (record-then-advance, \u00a73.1)",
        "epoch_pg/src/event_bus/mod.rs: ledger INSERT synchronous-confirmed for the SkipAfterBackstop path (fire-and-forget kept for fail-open); existing WARN + on_gap_timeout + HWM routing (:558-714) run unchanged; T7 pins ledger-write-failure \u2192 no advance"
      ],
      "requirements": [
        "R3",
        "R4"
      ],
      "tests": [
        "T2",
        "T3",
        "T7"
      ]
    },
    {
      "phase": 4,
      "focus": "Late-materialization detection and rebuild callback",
      "effort": "M",
      "difficulty": "standard",
      "id": "P4",
      "name": "Late-materialization detection + rebuild callback",
      "tdd": "T4 red \u2192 green",
      "scope": [
        "epoch_pg/src/event_bus/mod.rs: automate the gap_timeouts \u22c8 {events_table} JOIN (WHERE g.bus_name = $1) over unresolved rows for the bus's table; expose the detection core as a public on-demand entry point; fire a rebuild-needed callback on the GapTimeout callback surface (config.rs:163-241) and mark each hit row resolved (one callback per (subscriber, skipped seq)); auto-scan configurable/disableable; update the ReliableDeliveryConfig compatibility table"
      ],
      "requirements": [
        "R5"
      ],
      "tests": [
        "T4"
      ]
    },
    {
      "phase": 5,
      "focus": "WARN honesty, regression pins, docs",
      "effort": "S",
      "difficulty": "standard",
      "id": "P5",
      "name": "release_halt WARN honesty + regression pins + docs",
      "tdd": "T5, T6, T8 red \u2192 green",
      "scope": [
        "epoch_pg/src/event_bus/mod.rs: scope release_halt success WARN so it does not over-promise for ReplayAlways (block after :2290); no ReplayAlways recovery path added (OQ-3)",
        "rustdoc on GapPolicy incl. the \u00a73.4 in-flight-writer safety contract and the \u00a73.7 readiness amendment; CHANGELOG amends 0026 R5, clarifies (does not amend) 0027 \u00a73.3, and documents the new PgEventBusError variant; cargo fmt + clippy -D warnings; full DB-gated suite"
      ],
      "requirements": [
        "R6",
        "R7",
        "R8",
        "R9"
      ],
      "tests": [
        "T5",
        "T6",
        "T8"
      ]
    },
    {
      "phase": 6,
      "focus": "Counter-row migration + AllocationMode surface (Part B)",
      "effort": "S",
      "difficulty": "standard",
      "id": "P6",
      "name": "Counter-row migration + AllocationMode surface",
      "tdd": "T13 red \u2192 green (re-seed observable via direct SQL); migration verified green under the default Nextval path before P7",
      "scope": [
        "epoch_pg/src/migrations: new migration (m014) creating the per-table-keyed counter table epoch_events_sequence_counter(name TEXT PRIMARY KEY, val BIGINT NOT NULL); idempotent, additive; creates the table only \u2014 per-table rows are seeded by the constructor",
        "epoch_pg/src/event_store.rs: add AllocationMode enum (default Nextval | PerTxnCounter) and a new async FALLIBLE constructor (e.g. with_allocation_mode) that creates/seeds the per-table counter row (is_called-aware GREATEST seed floor, \u00a73.6) and FAILS construction on re-seed failure; default path byte-for-byte unchanged (no store-level config module exists; the spec's config.rs anchors are epoch_pg/src/event_bus/config.rs)"
      ],
      "requirements": [
        "R10",
        "R11"
      ],
      "tests": [
        "T13"
      ]
    },
    {
      "phase": 7,
      "focus": "PerTxnCounter write-path (Part B)",
      "effort": "M",
      "difficulty": "hard",
      "id": "P7",
      "name": "PerTxnCounter allocator write-path",
      "tdd": "T9, T10, T11, T12, T14, T15 red \u2192 green",
      "scope": [
        "epoch_pg/src/event_store.rs: under PerTxnCounter, store_events_in_tx/store_event (:144-152, :457-462) UPDATE the per-table-keyed counter by +K inside the insert txn and supply global_sequence explicitly; RETURNING global_sequence preserved; Nextval path untouched (column omitted, DEFAULT fires)",
        "zero-burn on rollback (counter rewinds) and crash (MVCC abort); no gap-machinery change \u2014 fence-safe by construction; T11 (default Nextval regression pin) is this phase's merge gate; T15 pins concurrent-writer contiguity"
      ],
      "requirements": [
        "R12",
        "R13",
        "R14"
      ],
      "tests": [
        "T9",
        "T10",
        "T11",
        "T12",
        "T14",
        "T15"
      ]
    },
    {
      "phase": 8,
      "focus": "Part B docs + consolidation (tests kept green)",
      "effort": "S",
      "difficulty": "standard",
      "id": "P8",
      "name": "Part B docs + AllocationMode consolidation",
      "tdd": "no new tests; T9\u2013T15 kept green",
      "scope": [
        "rustdoc on AllocationMode stating the fence-safety contract (\u00a73.6/\u00a75.2) and the write-tax trade-off (3.5-4.4x at K=1, alloc-cost Q2); CHANGELOG noting the opt-in and unchanged default; cargo fmt + clippy -D warnings; full DB-gated suite"
      ],
      "requirements": [
        "R15"
      ],
      "tests": []
    }
  ]
}
```
