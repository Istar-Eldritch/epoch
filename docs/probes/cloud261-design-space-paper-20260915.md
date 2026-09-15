# Probe Report: CLOUD-261 design space — sequence-burn resilience for ReplayAlways subscribers

- Date: 2026-09-15
- Repository: `/root/code/epoch-worktrees/cloud-261` (epoch Rust CQRS/event-sourcing framework)
- Branch / base commit: worktree `cloud-261`, HEAD = `bed756b` ("refactor(core,mem,pg): CLOUD-242 post-review fixes — trait invariant docs, mem dedup, test repeatability"), clean tree at probe start
- Mode: **paper test** — desk-check only; no repository mutation, no cargo runs. All evidence below is verbatim code/SQL/spec text read this session; line anchors were verified against HEAD `bed756b` and match the CLOUD-261 citations (no drift found on any cited anchor).

## Summary of verdicts

| Q | Verdict (one line) |
|---|---|
| Q1 | Allocation is **per-row** via a column `DEFAULT nextval('epoch_events_global_sequence_seq')` on `epoch_events.global_sequence`; production inserts are single-row `INSERT ... RETURNING global_sequence` (one per event, looped), and the test sentinel is the `cu_isolated_events_table` helper + a bare `SELECT nextval('{table}_seq')` burn. |
| Q2 | Sequence numbers surface in `Event.global_sequence: Option<u64>`, seq-ordered query APIs, and the `epoch_event_bus_checkpoints.last_global_sequence` u64 checkpoint; **option A changes no schema and only opt-in behavior**, **option B changes DDL/allocation but not the `Event` contract**; epoch_mem has **no** sequence numbers at all (documented asymmetry), so there is no parity constraint. |
| Q3 | No gap-policy field exists on `ReliableDeliveryConfig` (only `gap_timeout` + callbacks); the exact hook is the `FailureMode::FailClosed` arm of `advance_contiguous_checkpoint` (subscriber_state.rs:376-386) plus its caller site mod.rs:548-556; for ReplayAlways the HWM is **in-memory only** (`Arc<Mutex<HashMap<String,u64>>>`, never persisted), and `subscribe()` takes **no start position** — re-subscribe is always from zero. |
| Q4 | **No** current query ever looks below a subscriber position (all fetches are `WHERE global_sequence > $1`); the only below-observation is a documented operator SQL `JOIN epoch_event_bus_gap_timeouts ... ON e.global_sequence = g.skipped_sequence` — today a late row at a skipped seq is silently never delivered, and the existing shape suggests an automated version of that join as minimal detection. |
| Q5 | **No counter/lease/metadata row exists in the schema** to reuse (only the PG sequence itself); lock footprint: `nextval` holds a sequence-object lock only for the call, whereas `SELECT FOR UPDATE`+`UPDATE` on a counter row holds a row lock to txn end and serializes all writers; burn analysis: reserve-in-txn burns **zero** on rollback *and* crash (MVCC abort), the "batch boundary" burn the issue describes only arises in the committed-block + per-writer in-memory-cache variant (crash burns the unused tail ≤ block size). |
| Q6 | Spec 0026 fixed catch-up contiguity (R1-R7) and **proved the fence shows "writer finished, not aborted"**; spec 0027 fixed the live path and pinned the invariant "persisted checkpoint MAY lag `state.contiguous_checkpoint`, MUST NEVER lead it" — option A deliberately violates that invariant and must be opt-in; a reusable `on_gap_timeout`/`GapTimeoutInfo` hook (config.rs:163-241) and the `epoch_event_bus_gap_timeouts` table (m009, with unique key + indexes) already exist for audit/metrics. |

---

## Q1: Allocation today — DDL, nextval call sites, the test sentinel, per-row vs per-transaction

**Verdict:** Allocation is per-row, performed by the `epoch_events.global_sequence` column `DEFAULT nextval('epoch_events_global_sequence_seq')`; both production insert paths are single-row `INSERT`s that omit the column and `RETURNING global_sequence`, looped once per event inside the caller's transaction. The test sentinel is the `cu_isolated_events_table` helper (isolated table + own sequence) with an explicit `SELECT nextval(...)` burn.

**Method:** Read `epoch_pg/src/migrations/m001_create_events_table.rs`, `m002_add_global_sequence.rs`, `m004_rename_tables_with_epoch_prefix.rs`; grepped `nextval` across all of `epoch_pg/src`; read both production insert sites in `epoch_pg/src/event_store.rs` and the test helper region `epoch_pg/src/event_bus/mod.rs:4390-4740`.

**Evidence:**

*DDL (migrations).* `epoch_pg/src/migrations/m002_add_global_sequence.rs:31-42`:

```sql
CREATE SEQUENCE IF NOT EXISTS events_global_sequence_seq
```

```sql
ALTER TABLE events
ADD COLUMN IF NOT EXISTS global_sequence BIGINT
DEFAULT nextval('events_global_sequence_seq')
```

The same migration backfills row-by-row (`m002:53-70`, `SET global_sequence = nextval('events_global_sequence_seq')` per row), then `ALTER COLUMN global_sequence SET NOT NULL` (`m002:79`), `CREATE INDEX idx_events_global_sequence ON events(global_sequence)` (`m002:88-89`), and `ALTER SEQUENCE events_global_sequence_seq OWNED BY events.global_sequence` (`m002:98-99`). `epoch_pg/src/migrations/m004_rename_tables_with_epoch_prefix.rs:100` renames the sequence: `"ALTER SEQUENCE events_global_sequence_seq RENAME TO epoch_events_global_sequence_seq"`. The base table itself is created in `m001_create_events_table.rs:25-36` (no `global_sequence` there — added by m002).

*Every `nextval` site in `epoch_pg/src` (complete list from `grep -rn "nextval" epoch_pg/src`):*

| Site | Role |
|---|---|
| `migrations/m002_add_global_sequence.rs:42` | column DEFAULT (the production allocator) |
| `migrations/m002_add_global_sequence.rs:65` | one-time backfill UPDATE |
| `migrations/m004_rename_tables_with_epoch_prefix.rs:96-100` | sequence rename to `epoch_events_global_sequence_seq` |
| `event_bus/mod.rs:4529` | test helper repoints an isolated table's DEFAULT to its own sequence |
| `event_bus/mod.rs:4660` | test burn: `SELECT nextval('{table}_seq')` (test 1) |
| `event_bus/mod.rs:4735` | test burn: `SELECT nextval('{table}_seq')` (test 2) |
| doc comments only | `subscriber_state.rs:6`, `mod.rs:2362`, `mod.rs:2530`, `mod.rs:3458`, `mod.rs:4399` |

There is **no** Rust-side `nextval` in the production insert path — allocation happens implicitly in Postgres via the column DEFAULT.

*Production insert shape.* `epoch_pg/src/event_store.rs:146-152` (`store_events_in_tx`):

```rust
let insert_sql = format!(
    "INSERT INTO {} (id, stream_id, stream_version, event_type, data, \
     created_at, actor_id, purger_id, purged_at, causation_id, correlation_id, \
     schema_version) \
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
     RETURNING global_sequence",
    self.events_table,
);
for event in events { ... .fetch_one(&mut **tx).await?; ... }
```

`global_sequence` is absent from the column list (column DEFAULT assigns it) and `RETURNING global_sequence` hands the value back (`event_store.rs:187`: `global_sequence: Some(row.0 as u64)`). The identical statement is repeated for the single-event path `store_event` (`event_store.rs:457-462`). So: **per-row allocation** (one `nextval` per inserted row), executed inside whatever transaction the caller supplied (`store_events_in_tx` takes `tx: &mut sqlx::Transaction<'_, sqlx::Postgres>` and loops with `fetch_one(&mut **tx)`; `store_event` uses one implicit transaction per event via the pool). There is no multi-row INSERT and no batched allocation anywhere.

*Test sentinel helper (for the probe agent to reuse).* `epoch_pg/src/event_bus/mod.rs:4500-4515` — `cu_isolated_events_table(pool) -> String`. Its doc explains why it exists:

> "Creates a table shaped like `epoch_events` with its OWN sequence backing `global_sequence`, so a test can burn a `nextval()` to simulate a permanent hole without leaving one in the shared `epoch_events` table, which would stall every other test binary's catch-up and live loop (a brand-new subscriber with no planted checkpoint walks from sequence 0 and can hit it). `LIKE ... INCLUDING ALL` copies the DEFAULT expression byte-for-byte, so it still points at `epoch_events_global_sequence_seq` until repointed here; the new sequence is `OWNED BY` the column, so dropping the table drops it too. Caller is responsible for dropping the returned table at the end of the test."

Implementation (`mod.rs:4515-4540`): `CREATE TABLE {table} (LIKE epoch_events INCLUDING ALL)`; `CREATE SEQUENCE {seq}` (named `{table}_seq`); `ALTER TABLE {table} ALTER COLUMN global_sequence SET DEFAULT nextval('{seq}')`; `ALTER SEQUENCE {seq} OWNED BY {table}.global_sequence`. Companion helpers: `cu_insert` (`mod.rs:4543-4560`) — plain single-row `INSERT ... RETURNING global_sequence`; `cu_set_checkpoint` (`mod.rs:4579-4594`) — upserts `epoch_event_bus_checkpoints` to anchor a catch-up start. The burn itself, in `catchup_prefix_stops_at_hole` (`mod.rs:4635`), at `mod.rs:4655-4661`:

```rust
// Consume s3's sequence slot without inserting a row so it appears as
// a gap to catch_up_from_checkpoint. Using nextval on this table's OWN
// sequence rather than an open transaction avoids holding a table lock
// that would block concurrent TRUNCATE calls in other test binaries,
// and rather than the shared table's sequence avoids leaving a
// permanent hole anyone else could stumble on.
let s3: i64 = sqlx::query_scalar(&format!("SELECT nextval('{table}_seq')"))
```

(Repeated verbatim at `mod.rs:4729-4736` in `catchup_multi_page_hole_prefix_does_not_resume`, `mod.rs:4714`.)

**Confidence:** verified (all quotes read directly from HEAD `bed756b`).

**Implications:** Option B's allocator would replace the *only* production allocation mechanism (a column DEFAULT) — the two INSERT statements at `event_store.rs:146` and `event_store.rs:457` are the entire hook surface, and any explicit-allocation scheme must keep `RETURNING global_sequence` working. Option A touches none of this. The probe agent can reuse `cu_isolated_events_table` + `cu_insert` + the bare-`nextval` burn unchanged; the burn technique is deliberately chosen over an open transaction to avoid blocking sibling test binaries.

---

## Q2: Observable contracts — where sequence numbers surface, and what A/B would change

**Verdict:** `global_sequence` surfaces in `epoch_core::Event::global_sequence: Option<u64>`, in seq-ordered query APIs, in the `epoch_event_bus_checkpoints.last_global_sequence` checkpoint column, and in the public readiness/gap-timeout APIs; option A requires no schema or `Event` change (policy is opt-in per subscriber, skipped-seq state can be in-memory + the existing gap-timeouts table), option B changes the storage/allocation mechanics but not the `Event` field contract; epoch_mem allocates **no** sequence numbers at all (documented asymmetry), so there is no cross-backend parity to preserve.

**Method:** Read `epoch_core/src/event.rs` (field + builder), grepped `global_sequence` in `epoch_core/src`, `epoch_mem/src`, `epoch/src`; read `epoch_pg/src/event_bus/checkpoint.rs` in full; read the seq-ordered queries in `event_store.rs` and `mod.rs`; read the epoch_mem asymmetry table.

**Evidence:**

*The user-facing field.* `epoch_core/src/event.rs:66-73`:

```rust
/// Global sequence number assigned by the event store.
///
/// This is a monotonically increasing sequence number across all events in the store,
/// used for reliable event delivery, checkpointing, and catch-up processing.
/// It is `None` until the event is persisted to the event store.
pub global_sequence: Option<u64>,
```

Setter `EventBuilder::global_sequence(u64)` at `event.rs:416-418`. `epoch_core/src/aggregate.rs:778` documents it as backend-assigned ("fields populated (e.g., `global_sequence` for PostgreSQL)"). `epoch_core/src/causation.rs:43-44,134-141` sorts causal trees by `global_sequence` (`ORDER`-like contract: events with a sequence come before those without, stable otherwise).

*Query APIs ordered by seq.* `epoch_pg/src/event_store.rs:609-611` (correlation query): `FROM {} WHERE correlation_id = $1 ORDER BY global_sequence ASC`. Per-stream reads order by `stream_version` instead (`event_store.rs:416-420`). All five bus fetch queries filter `WHERE global_sequence > $1 ORDER BY global_sequence ASC LIMIT $2` (`mod.rs:853-857` private fetch, `mod.rs:1263`, `mod.rs:1972-1976` live shared fetch, `mod.rs:3546-3551` catch-up) and the subscribe drain uses `WHERE global_sequence > $1 AND global_sequence <= $2` (`mod.rs:4076`).

*Checkpoint format.* `epoch_pg/src/event_bus/checkpoint.rs:156-167`:

```sql
INSERT INTO epoch_event_bus_checkpoints (bus_name, subscriber_id, last_global_sequence, last_event_id, updated_at)
VALUES ($1, $2, $3, $4, NOW())
ON CONFLICT (bus_name, subscriber_id) DO UPDATE SET ...
```

Table DDL: `migrations/m003_create_event_bus_infrastructure.rs:63-68` (`last_global_sequence BIGINT NOT NULL DEFAULT 0`), PK widened to `(bus_name, subscriber_id)` by m008. Critically, `flush_checkpoint` is documented as a **blind, non-monotonic write** whose safety is entirely the caller's responsibility (`checkpoint.rs:131-145`):

> "# Hazard: this is a blind, non-monotonic write ... Every caller MUST pass only a value it is willing to publish as the subscriber's current position; this is exactly why the contiguous-prefix catch-up counter (spec 0026 R1/R2) and the live listener path (spec 0027 R1) never flush above a sequence they haven't proven contiguous."

*Public observability APIs over the sequence:* `PgEventBus::head_sequence` (`mod.rs:2363`, `SELECT MAX(global_sequence) FROM {}`), `subscriber_position` (`mod.rs:2449`), `subscriber_lag` (`mod.rs:2481`), `list_gap_timeouts` (`mod.rs:2922`), `resolve_gap_timeout` (`mod.rs:3046`), plus `GapTimeoutEntry` (`mod.rs:958+`).

*epoch_mem parity.* `grep global_sequence epoch_mem/src` → zero hits. `epoch_mem/src/event_store.rs:539-552` documents the asymmetry explicitly:

> "| Persistent checkpoints | ✅ (DB rows) | ❌ no persistence |
> | Global sequence numbers | ✅ | ❌ |
> | Gap detection / snapshot fencing | ✅ | ❌ |"

**Would option A change a public contract?** No schema change is forced: for a ReplayAlways subscriber the HWM and any skipped-seq set are per-process in-memory state (see Q3 — the HWM is `Arc<Mutex<HashMap<String,u64>>>`, "Never persisted"); durable skipped-seq audit can reuse `epoch_event_bus_gap_timeouts`, whose key `(bus_name, subscriber_id, skipped_sequence)` (`m009:39-41`) works for ReplayAlways subscribers even though they have no checkpoint row. A per-subscriber policy knob follows the established source-compatible pattern: `subscription_mode()`/`failure_mode()` are defaulted trait methods on `EventObserver` (`epoch_core/src/projection.rs:180-192`), and the config struct's "Source-compatibility" note (`config.rs:273-280`) already documents how added fields/defaults are handled. The one observable-contract change to document: readiness/lag semantics — spec 0026 R5 made a held hole block `wait_until_caught_up`; a SkipAfterBackstop subscriber would stop blocking once the hole is skipped (same class of observable change 0026 R5 itself made, requiring a CHANGELOG note).

**Would option B change a public contract?** `Event.global_sequence: Option<u64>` and "monotonically increasing" survive (block allocation preserves monotonicity, enlarges holes). What changes is storage mechanics: the column `DEFAULT nextval(...)` (`m002:41-42`) and/or the INSERT statements (`event_store.rs:146-152, 457-462`) must change to assign from a counter, plus a new/changed migration. The gap machinery (Q4/Q6) must remain either way — see Q5 for the residual-burn analysis. epoch_mem is unaffected (no sequences there).

**Confidence:** verified for all quoted code; the A/B contract assessments are derivations from that quoted code (labeled as such — the decision belongs to the spec).

**Implications:** Neither option breaks the `Event`/query surface. Option A's contract work is documentation + a defaulted trait/config knob; option B's is a migration + insert-path change, with `RETURNING global_sequence` (`event_store.rs:150`) preserved as the read-back contract.

---

## Q3: Option A extension points — config, hook site, HWM state, rebuild semantics

**Verdict:** There is no per-subscriber gap policy today — `ReliableDeliveryConfig` has only the bus-wide `gap_timeout` (default 5 s, config.rs:388) plus three callbacks; per-subscriber knobs today are the defaulted trait methods `subscription_mode()` and `failure_mode()`. The SkipAfterBackstop hook is the `FailureMode::FailClosed` arm of `advance_contiguous_checkpoint` (subscriber_state.rs:376-386) and its single caller site mod.rs:548-556 (`fire_on_halt(..., GapUnproven)`). The ReplayAlways HWM is in-memory only and never persisted; `subscribe()` takes no start position, so a rebuild is always a fresh `subscribe()` from zero.

**Method:** Read `epoch_pg/src/event_bus/config.rs:120-440`, `subscriber_state.rs:240-440`, `mod.rs:500-580`, `mod.rs:3789-3800` (subscribe), `epoch_core/src/event_store.rs:245-330` (trait + enums), `mod.rs:75-80, 1168-1180` (HWM fields).

**Evidence:**

*Subscriber/bus config.* `ReliableDeliveryConfig` (`config.rs:279+`) relevant fields, verbatim:

```rust
pub gap_timeout: Duration,                       // config.rs:307
pub on_gap_timeout: Option<Arc<dyn GapTimeoutCallback>>,   // config.rs:335
pub on_halt: Option<Arc<dyn HaltCallback>>,                // config.rs:351
pub snapshot_fencing: bool,                      // config.rs:377
```

Default: `gap_timeout: Duration::from_secs(5)` — `config.rs:388` (anchor matches the issue). There is **no** policy/enum field for gap handling; the struct's doc (config.rs:273-280) enumerates the fields ever added: `on_gap_timeout`, `on_halt`, `snapshot_fencing`.

*Existing per-subscriber enums.* `epoch_core/src/event_store.rs:289-299`:

```rust
pub enum FailureMode {
    #[default]
    FailOpen,
    FailClosed,
}
```

`epoch_core/src/event_store.rs:307-321`:

```rust
pub enum SubscriptionMode {
    #[default]
    Checkpointed,
    ReplayAlways,
}
```

Both are surfaced as defaulted trait methods on the observer — `epoch_core/src/projection.rs:180-192`:

```rust
fn subscription_mode(&self) -> crate::event_store::SubscriptionMode { ... Checkpointed }
fn failure_mode(&self) -> crate::event_store::FailureMode { ... FailOpen }
```

resolved once at subscribe and at listener wake (`mod.rs:3824-3831`, `mod.rs:1810-1818`).

*The exact hook (between backstop refusal and halt).* The resolver is `pub(crate) fn advance_contiguous_checkpoint(state, visible_seqs, gap_timeout, snapshot, failure_mode) -> AdvanceOutcome` (`subscriber_state.rs:306-313`), documented as "pure, synchronous, and DB-free: the snapshot bounds are passed in by the caller, which owns all DB/log/callback side-effects" (`subscriber_state.rs:296-298`). Its fence check — the anchor the issue cites as subscriber_state.rs:337-363 — is at `subscriber_state.rs:346-350`:

```rust
// FENCE (fast path): can we PROVE the gap is permanent? Every txn that
// was in-flight at detection has finished (xmin >= fence_xmax) and the
// sequence is still missing, so its writer aborted (or it was burned).
if let (Some(snap), Some(fence)) = (snapshot, fence_xmax)
    && snap.xmin >= fence
```

The backstop-refusal arm — the precise SkipAfterBackstop decision point — is `subscriber_state.rs:371-386`:

```rust
if gap_duration > gap_timeout {
    match failure_mode {
        FailureMode::FailClosed => {
            // Refuse the backstop: hold the gap, do not push a
            // SkippedGap, do not remove from gap_first_seen, do not
            // re-capture the fence. Fire on_halt only on first entry
            // (halt_fired guards per-batch spam).
            if !observation.halt_fired {
                observation.halt_fired = true;
                backstop_refused = Some(next);
            }
        }
        _ => {
            // Fail-open (and any future non-fail-closed mode): advance
            // past the gap as before (byte-for-byte unchanged).
            ... skipped.push(SkippedGap { skipped_sequence: next, gap_duration, reason: SkipReason::TimeoutBackstop, fence_xmax });
            state.gap_first_seen.remove(&next);
            state.contiguous_checkpoint = next;
            continue;
        }
    }
}
```

The caller side, in `process_subscriber_for_batch` (`mod.rs:300`), is `mod.rs:536-556`:

```rust
let outcome = advance_contiguous_checkpoint(
    &mut state, &visible_seqs, config.gap_timeout, snapshot, failure_mode,
);
// Spec 0028 P4 gap refusal: fire on_halt(GapUnproven) on the first batch
// cycle where the backstop would have fired but was refused. ...
if let Some(refused_seq) = outcome.backstop_refused {
    fire_on_halt(&config, &subscriber_id, refused_seq, HaltReason::GapUnproven).await;
}
```

This is the only `HaltReason::GapUnproven` site in the codebase (`grep -n "HaltReason::GapUnproven"` → `mod.rs:553` only), confirming the issue's "fires from the live-batch path only". A `SkipAfterBackstop` mode would (a) thread a new policy value into `advance_contiguous_checkpoint` alongside `failure_mode` and convert the refused gap into a `SkippedGap { reason: SkipReason::TimeoutBackstop, ... }` + `state.contiguous_checkpoint = next` exactly as the fail-open arm already does, and (b) let the caller's existing downstream machinery run: the partition at `mod.rs:561-563`, the WARN "Gap timeout: advancing ... if any writing transaction later commits, the event will NOT be delivered to this subscriber (recorded in epoch_event_bus_gap_timeouts)" (`mod.rs:595-604`), the fire-and-forget `INSERT INTO epoch_event_bus_gap_timeouts ... ON CONFLICT ... DO NOTHING` + `on_gap_timeout` callback (`mod.rs:636-702`), and the ReplayAlways HWM routing at `mod.rs:708-713`:

```rust
if replay_always {
    // Route advancement to the in-memory HWM; never touch the
    // checkpoints table for a ReplayAlways subscriber.
    hwm.lock().await.insert(subscriber_id.clone(), new_contiguous);
}
```

*Where the HWM/checkpoint is persisted.* For Checkpointed subscribers: `SubscriberState.contiguous_checkpoint` — "This is the value persisted to the database checkpoint" (`subscriber_state.rs:99-103`) — flushed via `flush_checkpoint` into `epoch_event_bus_checkpoints.last_global_sequence` (Q2). For ReplayAlways: **nothing is persisted**. The bus field (`mod.rs:1173-1178`):

```rust
/// Per-subscriber in-memory high-water mark for [`SubscriptionMode::ReplayAlways`]
/// subscribers. Never persisted: a crash loses it and the next boot replays
/// from 0, which is the intended contract. Shared across `Clone`s so
/// `subscribe`, the listener task, and readiness queries observe the same value.
hwm: Arc<Mutex<HashMap<String, u64>>>,
```

and the batch context carries the same map (`mod.rs:75-78`). This is why P4b self-heal excludes ReplayAlways — `mod.rs:1884-1893`:

> "`ReplayAlways` wedges are out of P4b's scope (no persisted checkpoint row to re-seed from; the remedy is a fresh `subscribe()`, R9b) — the `ReplayAlways` floor-exclusion analogue lands in P5. This is the documented, bounded intra-pipeline gap."

So "advance HWM past the hole" for ReplayAlways = the in-memory map insert above (live) or `advance_catchup_prefix`'s guard (catch-up, `mod.rs:3406-3425`); "track skipped seqs" has no persisted home today — either new in-memory per-subscriber state (lost on restart, consistent with ReplayAlways's contract) or reuse of the gap-timeouts table for durable audit.

*Rebuild semantics / subscribe signature.* Trait definition, `epoch_core/src/event_store.rs:246-252`:

```rust
/// Allows to subscribe to events
fn subscribe<T>(
    &self,
    projector: T,
) -> Pin<Box<dyn Future<Output = Result<(), Self::Error> + Send>>>
where
    T: EventObserver<Self::EventType> + Send + Sync + 'static;
```

PgEventBus impl at `mod.rs:3789-3798` — same shape, **no start-position parameter**. The `SubscriptionMode::ReplayAlways` doc (`epoch_core/src/event_store.rs:312-320`) specifies rebuild semantics:

> "# Caveat: re-subscribing does not rebuild your model for you
> A backend that implements this mode resets its own high-water mark to 0 on every `subscribe()` call, including calling `subscribe()` again with a `subscriber_id` already in use, so catch-up replays the full history again. If you pass in an observer that wraps a model instance which **survived** from a previous subscription ..., that model will have the same history applied to it twice. Build a fresh model before every `subscribe()` call..."

Catch-up for ReplayAlways starts from the *surviving in-memory HWM* (not the checkpoint table): `mod.rs:3502-3508` — "let last_sequence = if replay_always { hwm.lock().await.get(subscriber_id).copied().unwrap_or(0) } else { ... SELECT last_global_sequence FROM epoch_event_bus_checkpoints ... }". So: fresh process → HWM 0 → full replay; listener restart in-process → resumes from retained HWM; a "rebuild" is achieved by a fresh `subscribe()` with a fresh model, never by passing a position.

**Confidence:** verified.

**Implications:** The spec can name the hook precisely: resolver arm `subscriber_state.rs:376-386` (policy input threaded like `failure_mode`, since the resolver is pure) + caller site `mod.rs:548-556` (where halt currently fires; a SkipAfterBackstop outcome instead flows into the existing `timeout_backstop` handling so logging/recording come free). State for "advance past hole" already exists in both modes (in-memory `contiguous_checkpoint`/HWM); only the skipped-seq set is genuinely new state.

---

## Q4: Late-materialization detection — does anything observe a row below the HWM?

**Verdict:** No. Every bus fetch query is `WHERE global_sequence > $1` (strictly above the shared floor / cursor); the only mechanism in-tree that can observe a committed row below a position is a documented **operator SQL join** of `epoch_event_bus_gap_timeouts` against the events table — and a late row at an already-skipped seq is, today, silently never delivered. The existing shape (gap-timeouts table + its `skipped_sequence` index + `resolve_gap_timeout`) suggests automating exactly that join as the minimal detection.

**Method:** Grepped all `global_sequence` comparison operators in `mod.rs`; read the shared live fetch + `compute_shared_floor`, the catch-up query, the drain query, `GapTimeoutEntry` docs, and the three cited comments (mod.rs:2362, 2530, 3458); read spec 0026 §3 for the structural argument.

**Evidence:**

*The poll/catch-up WHERE clauses.* Catch-up (`mod.rs:3544-3551`): `FROM {} WHERE global_sequence > $1 ORDER BY global_sequence ASC LIMIT $2`. Live shared fetch, floored at the minimum contiguous checkpoint of non-wedged subscribers (`mod.rs:1962-1976`):

```rust
let min_checkpoint = match compute_shared_floor(subscriber_states.values()) { Some(floor) => floor, None => break };
let catchup_query = format!("... FROM {} WHERE global_sequence > $1 ORDER BY global_sequence ASC LIMIT $2", ...);
let rows ... .bind(min_checkpoint as i64) ...
```

`compute_shared_floor` = "the minimum contiguous checkpoint across all NON-wedged subscribers" (`mod.rs:1965-1968`; implementation `subscriber_state.rs:223-246`). Private wedged-fetch (`mod.rs:853-857`) and the subscribe drain (`mod.rs:4076`, `WHERE global_sequence > $1 AND global_sequence <= $2`) have the same shape. The complete operator inventory from `grep -n "global_sequence <\|<=\|>\|>=" epoch_pg/src/event_bus/mod.rs` contains **no** query with a `global_sequence <` / `<=` predicate other than the drain's upper bound — nothing re-reads below a position.

*What the three cited comments already anticipate.*

`mod.rs:2361-2367` (`head_sequence` doc):

> "Note: with non-transactional `nextval` (spec 0019) the head may be a burned/in-flight value that never becomes visible, because the transaction that reserved it rolled back or is still open. Readiness does not wait on such a value directly: the listener's gap-timeout mechanism advances a subscriber's contiguous checkpoint past a sequence like this once it has been missing long enough to conclude it never will become visible."

`mod.rs:2527-2534` (`wait_until_caught_up` doc):

> "# Hazard: burned/in-flight tail
> `target` is `head_sequence()`, which can be a burned or in-flight `nextval` (spec 0019) that only ever becomes visible via the gap backstop. When the tail is such a sequence, `position` cannot reach `target` until the gap timeout elapses, so callers must pass a `timeout` larger than the subscriber's `gap_timeout` to avoid a spurious `Ok(false)`."

`mod.rs:3452-3466` (`catch_up_from_checkpoint` doc):

> "... paginates `global_sequence > checkpoint` in `catch_up_batch_size` chunks ... `global_sequence` is assigned by a non-transactional `nextval()` (spec 0019), so a visible page can contain a hole a still-open transaction fills in later; checkpointing the maximum would strand that event below the seed of the live loop. The prefix freezes at the first hole ... A hole that never fills leaves the checkpoint below it, so the live loop re-reads from there and delivers it ... Recovering a *permanent* hole (one that will never fill) is the live listener's job, not this pass's: catch-up has no gap fence, snapshot, or timeout backstop of its own."

So the comments anticipate: (a) burned/in-flight *head* values, (b) readiness gates needing `timeout > gap_timeout`, (c) holes that fill later while the checkpoint is *held below* them — in which case the live loop's `> min_checkpoint` query re-reads from below the hole and delivers the late row (spec 0026 §3: "The live path guarantees this by re-querying from `min_checkpoint` (below the gap) every batch"). What they do **not** provide is any observation below a position that has already *advanced past* a hole.

*What happens today if a row lands at a skipped seq after the position advanced.* It is never delivered, and the code says so verbatim — the WARN at `mod.rs:597-604`:

> "Gap timeout: advancing '{}' on bus '{}' past {} missing sequence(s) — {} — **if any writing transaction later commits, the event will NOT be delivered to this subscriber (recorded in epoch_event_bus_gap_timeouts)**"

Once the skip happens, `min_checkpoint` (live) and the cursor (catch-up) sit above the hole, so every fetch is structurally blind to the late row.

*The existing below-observation mechanism — the operator join.* `GapTimeoutEntry` doc, `mod.rs:958-970`:

> "A recorded gap-timeout: a global sequence a subscriber's checkpoint advanced past because the gap did not fill within `gap_timeout`. ... If the skipped sequence later turns out to have committed (use `SELECT g.* FROM epoch_event_bus_gap_timeouts g JOIN epoch_events e ON e.global_sequence = g.skipped_sequence` to detect this), the operator can replay the event and then call `PgEventBus::resolve_gap_timeout` to mark the record as resolved."

Supporting infrastructure already in place: `m009` creates `epoch_event_bus_gap_timeouts` with `UNIQUE (bus_name, subscriber_id, skipped_sequence)` (`m009:39-41`), `CREATE INDEX idx_epoch_gap_timeouts_sequence ON epoch_event_bus_gap_timeouts (skipped_sequence)` (`m009:47-50`), and a partial index `... ON (resolved_at) WHERE resolved_at IS NULL` (`m009:55-59`); `resolve_gap_timeout` (`mod.rs:3046-3064`) is `UPDATE ... SET resolved_at = NOW() ... WHERE id = $1 AND resolved_at IS NULL`.

**Confidence:** verified — the absence of any `<`-predicated fetch query and the presence of the documented join are both directly quoted.

**Implications:** Option A's late-materialization detection has a natural, already-shaped minimal form: periodically run the documented JOIN (unresolved gap-timeout rows ⋈ events on `skipped_sequence`) and trigger a rebuild/notification on a hit — every ingredient (table, unique key, indexes, unresolved filter, resolve workflow) exists. What does not exist is any *automatic* below-HWM observer; a spec adding one would be new machinery, not a reuse.

---

## Q5: Option B mechanics — reusable counter row, lock footprint, burn semantics

**Verdict:** The schema contains **no** counter/lease/metadata row a batched allocator could reuse — the only allocator is the PostgreSQL sequence itself. Lock footprint: a column-DEFAULT `nextval` takes its sequence lock only for the duration of the call (concurrent writers never block each other), whereas `SELECT ... FOR UPDATE` + `UPDATE` on a counter row inside the insert transaction holds a row lock until txn end and serializes every event-writing transaction on one hot row. Burn derivation, both variants: (1) *reserve-inside-the-insert-txn* — rollback and crash both burn **zero** (MVCC abort discards the uncommitted counter UPDATE); (2) *committed-block + per-writer in-memory cache* — an insert-txn rollback burns exactly the sequence values that txn drew (already committed to the counter), and a writer crash burns the entire unused cache tail, i.e. at most the batch boundary — this is the variant the issue's "burns at most a batch boundary and only on crash" describes.

**Method:** Grepped all 14 migration files for tables/columns (full inventory); grepped `advisory` for existing lock machinery; derived the SQL/MVCC semantics from the in-tree statements about `nextval` (m011 doc, specs/0012, mod.rs:4399, specs/0019) plus standard PostgreSQL row-lock/MVCC rules. No DB was touched (paper test); the probe agent is invited to measure both variants.

**Evidence:**

*Full schema inventory (migrations m001-m013).* Tables created: `events`→`epoch_events` (m001, m004), `event_bus_checkpoints`→`epoch_event_bus_checkpoints` (m003, m004, PK widened to `(bus_name, subscriber_id)` in m008), `event_bus_dlq`→`epoch_event_bus_dlq` (m003), `epoch_event_bus_gap_timeouts` (m009), `epoch_snapshots` (m013), `_epoch_migrations` (bookkeeping). Plus one PostgreSQL **sequence** `events_global_sequence_seq`→`epoch_events_global_sequence_seq` (m002, m004) and column DEFAULTs on `epoch_events`: `global_sequence DEFAULT nextval(...)` (m002) and `txid DEFAULT (pg_current_xact_id()::text::bigint)` (m011). Nothing resembling a monotonic counter row, lease row, or allocator metadata row exists. The checkpoints table is per-subscriber keyed state, not a global counter; advisory locks (`mod.rs:3071-3110`, `SELECT pg_try_advisory_lock(('x' || substr(md5($1),1,8))::bit(32)::int, ...)`) are session-scoped coordination locks, not data rows. m011's `txid` DEFAULT is precedent for per-row column-DEFAULT identity derived from transaction metadata — but it observes the txn id, it does not allocate sequence space.

*Lock footprint — in-tree statements about nextval.* `mod.rs:4396-4400`:

> "`SELECT nextval(...)` without inserting a row, so no transaction is held"

`specs/0012-out-of-order-notify-checkpoint-skip-bug.md:12,325-326` establishes that two concurrent transactions draw N and N+1 in parallel:

> "PostgreSQL's `nextval()` is non-transactional: two concurrent transactions can obtain sequences N and N+1 but commit in reverse order." / "Tx A: `nextval()` → N, sleep 200ms, INSERT, COMMIT" / "Tx B: `nextval()` → N+1, INSERT, COMMIT immediately"

`specs/0019-cloud180-snapshot-fencing.md:17-18`: "Because PostgreSQL sequences (`nextval`) are **non-transactional", a [burned value may never become visible]". The derived contrast (standard SQL semantics, labeled as derivation): a sequence object's internal lock is taken and released *within* the `nextval` call — writers proceed concurrently and the value is not undone on rollback; a counter-row `SELECT FOR UPDATE` takes a **tuple lock held to transaction end**, so with T concurrent writers each insert transaction serializes behind the previous one's *entire* duration (including its non-insert work), turning the counter row into a hot spot with associated row-version churn. This is precisely the "counter-row contention (nextval is lock-free precisely to be fast)" cost named in the issue.

*Burn derivation, stated precisely for the probe agent to measure:*

- **Variant 1 — reserve inside the insert txn.** Allocator does `UPDATE epoch_seq_counter SET next = next + $N RETURNING next ...` (or `SELECT ... FOR UPDATE`) in the *same* transaction as the inserts; the txn consumes values `[next, next+N)`. A **rollback** aborts the whole transaction: the counter's row update is an uncommitted MVCC version and is discarded — the counter rewinds to `next`, **zero sequences burn**, and the reserved range is reusable. A **writer crash** kills the backend; Postgres aborts the in-doubt transaction during recovery — likewise **zero burn**. Residual gaps in this variant come only from non-contiguous multi-row usage patterns, not from rollback/crash. Cost: full serialization on the counter row (above) plus one extra row touch per txn.
- **Variant 2 — committed block + per-writer cache.** A writer reserves a block of N in one (committed) transaction, persists nothing per insert, and keeps the unused tail in process memory across subsequent insert transactions. A **rollback** of an insert transaction burns exactly the values that transaction drew from the cache (they were already committed to the counter; the cache slot is consumed) — bounded by the per-txn draw, up to N. A **crash while holding a cached block** burns the entire unused tail — at most N−1, i.e. "at most a batch boundary", and only on crash. This variant keeps writer concurrency (the counter is touched once per block, not per event) at the price of a permanent small burn window; the epoch gap machinery (Q4/Q6) remains necessary in *both* variants, but variant 1 shrinks observable gaps to ~zero while variant 2 keeps bounded ones.

**Confidence:** schema inventory and nextval citations: **verified** (quoted). The lock-footprint and burn semantics: **partial** — derived from standard SQL/MVCC semantics plus the quoted in-tree statements, not measured this session (paper test mode forbids the experiment; the probe agent is expected to measure exactly the two variants above).

**Implications:** The spec must budget a new counter row/table for option B (nothing reusable exists), and should state which variant it means — the two have opposite burn/lock trade-offs: variant 1 is truly burnless on rollback *and* crash but serializes writers; variant 2 preserves concurrency but reintroduces bounded burns on crash (and per-txn-draw burns on rollback), so option B does **not** eliminate the gap machinery either way.

---

## Q6: Prior art in-tree — spec 0026 / 0027 decisions, and the gap-timeout hook + table

**Verdict:** Spec 0026 (CLOUD-226) decided catch-up must never persist above a hole and proved structurally that the fence means "writer finished, not aborted" — invalidating any SkipAfterBackstop reasoning that treats a cleared fence as proof of abort outside a current view of the gap region; spec 0027 (CLOUD-232) decided the live path may lag but never lead the contiguous prefix and that the two advancers stay separate — option A's advance-past-hole deliberately makes the published position *lead* the delivered prefix, so it must be an explicit, opt-in carve-out from both specs' invariants. The reusable audit/metrics machinery exists: the `on_gap_timeout` hook (`config.rs:163-241`) and the `epoch_event_bus_gap_timeouts` table (m009).

**Method:** Read `specs/0026-cloud226-contiguous-catchup-checkpoint.md` and `specs/0027-cloud232-live-path-contiguous-checkpoint.md` in full; read `config.rs:55-241` (HaltReason/HaltInfo/GapTimeoutInfo/GapTimeoutCallback) and `migrations/m009_create_gap_timeout_log.rs` in full.

**Evidence:**

*Spec 0026 decisions option A must respect.* §2 Non-Goals: "No change to the live path's gap machinery, public API, error types, or schema." §3 (the structural warning):

> "Giving catch-up a `SubscriberState` and calling `advance_contiguous_checkpoint` per page is unsound. That function's unstated precondition is that the visible-sequence set is *complete* from `contiguous_checkpoint` upward. The live path guarantees this by re-querying from `min_checkpoint` (below the gap) every batch. Catch-up cannot: its pagination cursor must advance past the hole (§4.1), so later pages query `> cursor` and structurally cannot re-observe the gap even after it commits. The resolver then sees a permanent hole and, once `xmin` passes the captured `fence_xmax`, reports `SkipReason::FenceCleared` — a *committed* event skipped permanently, more quietly than the original bug. **The fence proves the writer finished, not aborted**; that inference is only valid against a current view of the gap region."

Requirements: "R1. `catch_up_from_checkpoint` MUST NOT persist a checkpoint above a hole. R2. The drain MUST NOT flush above the contiguous value carried from catch-up. R3. Both MUST terminate when a hole is held open for the whole pass. R4. A checkpoint's `last_event_id` MUST match its `last_global_sequence`. R5. Readiness MUST NOT report caught-up while a checkpoint is legitimately held below a hole. R6. `ReplayAlways` unchanged. R7. No public API, error-type, or schema change." §4.5: "`ReplayAlways` unchanged, still wrong ... Resuming from a surviving linear-max HWM means such a subscriber above a hole still misses the event on in-process restart. Tracked in CLOUD-227."

*Spec 0027 decisions option A must respect.* §2: "`advance_contiguous_checkpoint` (`subscriber_state.rs`) is the authoritative, correct advancer ... **The two advancers stay separate.** `advance_catchup_prefix` is a fence-less single-event exact-match advance by design (its pagination cursor cannot re-observe a gap). Merging was rejected." §3.3 — the invariant:

> "**After a flush from any site, the persisted checkpoint MAY lag `state.contiguous_checkpoint`; it MUST NEVER lead it.**"

R1: "Live path MUST NOT persist a checkpoint above an unproven-contiguous sequence, including via both bulk flushers." R6: "`ReplayAlways` subscribers write no checkpoint from this path." R7: "No public API / error-type / schema / migration change." §6: "CHANGELOG scoped accordingly ... Do not claim a steady-state change on the default mode."

A SkipAfterBackstop policy is a deliberate inversion of 0027 §3.3/R1 for opt-in subscribers (the published position leads the delivered prefix by the size of the skipped hole) and of 0026 R5 (readiness stops blocking). Both specs' fixes exist to prevent silent loss; option A re-introduces bounded, *recorded* silent loss — hence the opt-in framing in the issue and the audit trail below. The 0026 §3 fence caveat also directly addresses the issue's open question about the in-flight-writer case: fence evidence alone (xmin ≥ fence_xmax) proves the *original* gap writer finished, but a *skipped-seq* writer that starts after the fence was captured is not covered by that fence at all — detection must come from the Q4 join, not the fence.

*The reusable hook.* `epoch_pg/src/event_bus/config.rs:163-184` (`GapTimeoutInfo`):

```rust
pub struct GapTimeoutInfo {
    pub bus_name: String,
    pub subscriber_id: String,
    pub skipped_sequence: u64,
    pub gap_duration: Duration,
}
```

`config.rs:186-241` (`GapTimeoutCallback`):

> "Callback invoked when a subscriber's checkpoint is advanced past a gap due to timeout. ... The callback fires after the gap-timeout record has been persisted; errors or panics inside the callback are isolated in a detached task and do **not** affect checkpoint advancement — the gap is always skipped regardless of callback outcome."

```rust
#[async_trait]
pub trait GapTimeoutCallback: Send + Sync {
    /// Called after a gap-timeout record has been persisted to
    /// `epoch_event_bus_gap_timeouts`.
    async fn on_gap_timeout(&self, info: GapTimeoutInfo);
}
```

Wiring on `ReliableDeliveryConfig`: `pub on_gap_timeout: Option<Arc<dyn GapTimeoutCallback>>` (`config.rs:335`), default `None` (`config.rs:395`). Fire site: `mod.rs:636-702` — fire-and-forget task that first persists:

```sql
INSERT INTO epoch_event_bus_gap_timeouts
    (bus_name, subscriber_id, skipped_sequence, gap_duration_ms)
VALUES ($1, $2, $3, $4)
ON CONFLICT (bus_name, subscriber_id, skipped_sequence) DO NOTHING
```

then invokes the callback "Only ... when a NEW record was inserted" (`mod.rs:673-676`).

*The table.* `migrations/m009_create_gap_timeout_log.rs:30-44`:

```sql
CREATE TABLE IF NOT EXISTS epoch_event_bus_gap_timeouts (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    bus_name         VARCHAR(255) NOT NULL,
    subscriber_id    VARCHAR(255) NOT NULL,
    skipped_sequence BIGINT       NOT NULL,
    gap_duration_ms  BIGINT       NOT NULL,
    timed_out_at     TIMESTAMPTZ  NOT NULL DEFAULT NOW(),
    resolved_at      TIMESTAMPTZ,
    resolved_by      VARCHAR(255),
    resolution_notes TEXT,
    CONSTRAINT uq_epoch_gap_timeouts_bus_sub_seq
        UNIQUE (bus_name, subscriber_id, skipped_sequence)
)
```

with `idx_epoch_gap_timeouts_sequence` on `(skipped_sequence)` and a partial index on unresolved rows (m009:47-59). Its header doc already frames the late-commit workflow: "The `resolved_at`, `resolved_by`, and `resolution_notes` columns mirror the DLQ resolution workflow introduced in m006, enabling operators to record whether the skipped event was confirmed rolled-back or committed late and replayed via out-of-band means" (m009:6-9).

One boundary to note for the spec: today this machinery runs **only** for gaps actually skipped — i.e. fail-open backstop (`SkipReason::TimeoutBackstop`) — while "Fence-cleared gaps are proven permanent with no data loss: a single debug line, no WARN / record / callback" (`mod.rs:565-567`; `SkipReason::FenceCleared` doc: "No data loss; NOT recorded as a gap-timeout", `subscriber_state.rs:33-39`). A ReplayAlways + FailClosed subscriber currently reaches *neither* path (it refuses at `subscriber_state.rs:376-386` and wedges). So a SkipAfterBackstop policy would be the first thing that routes a ReplayAlways subscriber into the record + callback path — which is exactly the reuse option A wants, but the spec must decide whether fence-cleared skips under the new policy are also recorded (today they deliberately are not).

**Confidence:** verified.

**Implications:** Option A can be specced as: new opt-in policy knob → resolver arm `subscriber_state.rs:376-386` emits a `TimeoutBackstop`-style `SkippedGap` → existing WARN + `epoch_event_bus_gap_timeouts` record + `on_gap_timeout` callback run unchanged (audit/metrics for free) → HWM/contiguous advance via the existing ReplayAlways routing (`mod.rs:708-713`) → skipped-seq set + automated Q4 join as the rebuild trigger. It must explicitly amend 0027 §3.3/R1 and 0026 R5 for opt-in subscribers and address the 0026 §3 fence caveat for the in-flight-writer case.

---

## Notes

Related observations noticed while desk-checking, not chased further (design decisions belong to the spec):

1. **`release_halt` is prior art for audited forward skips** (`mod.rs:2306-2353`): an operator release already implements "advance past an unprocessed sequence + WARN + `HaltReason::Released` callback", but it is Checkpointed-only (reads/writes the checkpoint row; `BackwardRelease` doc points ReplayAlways at "a fresh `subscribe()`", `mod.rs:1018-1023`). A SkipAfterBackstop policy is essentially the automatic, per-subscriber analogue of this, with `epoch_event_bus_gap_timeouts` as its audit table.
2. **Fence-cleared gaps for a wedged ReplayAlways subscriber can still self-heal via the private path** (`mod.rs:887-893`: the private fetch snapshot gate is `config.snapshot_fencing && !state.gap_first_seen.is_empty()` "so a gap-wedged subscriber's `FenceCleared` recovery can still fire from the private path") — except that the P4b private pass itself excludes ReplayAlways subscribers (`mod.rs:1890-1894`). So a ReplayAlways wedge can only clear if a future P5 lands; today the wedge is permanent for the process lifetime, consistent with the issue's premise.
3. The issue's gap-fence anchor (subscriber_state.rs:337-363) and all other cited anchors (mod.rs:544-556, 1884-1893, config.rs:388, helper 4505-4529) matched HEAD `bed756b` exactly — no drift.

## Residual risks

- Q5's burn/lock analysis is derived, not measured (paper-test mode). The probe agent should measure: (a) nextval vs counter-row throughput under concurrent writers, (b) zero-burn on rollback for variant 1, (c) block-tail burn on crash for variant 2.
- The fence-vs-in-flight-writer semantics for a *skipped* sequence (issue's open question) are answered here only as far as the specs go: the fence covers writers in flight at gap detection (mod.rs:620-634 persistent-pin diagnostic covers the pinned case); a writer that begins *after* the fence was captured is outside every current mechanism and would only be caught by the Q4 join.
- Whether `epoch_event_bus_gap_timeouts` rows are acceptable as a ReplayAlways subscriber's durable skipped-seq record is a design decision (they outlive the process; the ReplayAlways contract says nothing persists today).
