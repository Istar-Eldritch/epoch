# Probe Report: CLOUD-261 failure chain — burn → gap → fence/backstop → GapUnproven → ReplayAlways wedge

- Date: 2026-09-15 (report filename date per task)
- Repository: `/root/code/epoch-worktrees/cloud-261` (epoch Rust CQRS/event-sourcing framework)
- Branch / base commit: worktree at HEAD `bed756bc22f76bdd7e8ea68cc436f82b635fc431`, clean tree (`git status --porcelain` empty at start; matches the rev CLOUD-261 cites)
- Mode: **paper test** (desk-check only; no repository mutation, no cargo runs; instrument = reading code + git history)
- Path conventions (as given by the task): `mod.rs` = `epoch_pg/src/event_bus/mod.rs`, `subscriber_state.rs` = `epoch_pg/src/event_bus/subscriber_state.rs`, `config.rs` = `epoch_pg/src/event_bus/config.rs`. All line anchors below were re-verified this session against HEAD `bed756b`.

## Summary (one-line verdicts)

| Q | Verdict |
|---|---|
| Q1 | **Verified.** `global_sequence` comes from the column `DEFAULT nextval('epoch_events_global_sequence_seq')` (m002 + m004 rename); both production INSERTs omit the column so the DEFAULT fires per row inside the caller's insert transaction; PostgreSQL sequence ops are non-transactional, and the repo itself attests (comment + integration-test doc) that a rollback permanently burns the slot. |
| Q2 | **Verified.** `HaltReason::GapUnproven` has exactly one production construction site — `mod.rs:553`, inside `process_subscriber_for_batch` (fn at `mod.rs:300`). Catch-up (`catch_up_from_checkpoint`, `mod.rs:3492`) has no fence/snapshot/backstop for gaps. Nuance: `process_subscriber_for_batch` is also invoked by the P4b private fetch (Checkpointed-only), so the single site is reachable from that path too — but for a ReplayAlways subscriber the shared live batch is indeed the only firing path. |
| Q3 | **Verified.** The CLOUD-227 guard (`mod.rs:3410-3425`) pins the ReplayAlways HWM below the first hole; events above the hole are still delivered but never advance the HWM; the pagination cursor advances so catch-up terminates; nothing later advances the HWM past an unproven hole (exhaustive HWM write sites: `mod.rs:712`, `mod.rs:3421`, `mod.rs:3920`; `head_sequence` is read-only). |
| Q4 | **Verified.** P4b self-heal covers wedged fail-closed **Checkpointed** subscribers only; ReplayAlways is excluded at `mod.rs:1888-1893` with the verbatim "the remedy is a fresh `subscribe()`" comment. `release_halt` (`mod.rs:2290`) upserts the checkpoint row forward and fires `HaltReason::Released`; epoch itself never calls it — only integration tests do (it is a public operator API). |
| Q5 | **Verified.** Fence at `subscriber_state.rs:349-350` requires `snap.xmin >= fence_xmax`; `gap_timeout` field at `config.rs:307`, default `Duration::from_secs(5)` at `config.rs:388`. Chain: gap armed → fence evaluated each batch → backstop after 5 s → FailClosed refusal → `fire_on_halt(GapUnproven)` → wedge (excluded from all fetch paths). **Nothing is persisted on the GapUnproven path** — no halt flag on disk, no `epoch_event_bus_gap_timeouts` row (that table is fail-open TimeoutBackstop only). |
| Q6 | **Verified.** A ReplayAlways GapUnproven wedge persists nothing (no checkpoint row, no gap-timeout row, no DLQ row; HWM is "Never persisted", `mod.rs:1175-1179`). On restart, nothing re-reads halt state (none exists to read); the app must re-`subscribe()`, which zeroes the HWM and replays from 0, re-detects the gap, and re-wedges after ~5 s if the fence still cannot prove. In-process auto-recovery exists only for Checkpointed (P4b private fetch / re-attempt / release reconciliation); **none** for a ReplayAlways gap wedge. |

---

## Q1: Burn source — where do `global_sequence` values come from, and what wraps the insert?

**Verdict:** `global_sequence` is assigned by the column DEFAULT `nextval(...)`; both production INSERT statements omit the column, so PostgreSQL evaluates the DEFAULT inside the caller's insert transaction; sequence increments are non-transactional, so a rolled-back transaction permanently burns every slot it consumed. Verified against code and in-repo attestations.

**Method:** Read the migration chain (m002, m004) and both production insert paths in `epoch_pg/src/event_store.rs`, the transaction wrappers, the transactional-aggregate glue in `epoch_pg/src/aggregate.rs`, and grepped the whole crate for `nextval`.

**Evidence:**

The DEFAULT (migration m002; the sequence is later renamed by m004 — `ALTER SEQUENCE ... RENAME` carries the column default with it since defaults reference the sequence by OID):

```sql
-- epoch_pg/src/migrations/m002_add_global_sequence.rs:38-45
        sqlx::query(
            r#"
            ALTER TABLE events
            ADD COLUMN IF NOT EXISTS global_sequence BIGINT
            DEFAULT nextval('events_global_sequence_seq')
            "#,
        )
```

```sql
-- epoch_pg/src/migrations/m004_rename_tables_with_epoch_prefix.rs:90,100
            sqlx::query("ALTER TABLE events RENAME TO epoch_events")
...
                "ALTER SEQUENCE events_global_sequence_seq RENAME TO epoch_events_global_sequence_seq",
```

Production insert #1 — `store_events_in_tx` (the batch path used by the transactional aggregate). Note the column list has **no `global_sequence`**, so the DEFAULT fires; `RETURNING global_sequence` reads back the DEFAULT-assigned value:

```sql
-- epoch_pg/src/event_store.rs:146-151 (fn at :135)
        let insert_sql = format!(
            "INSERT INTO {} (id, stream_id, stream_version, event_type, data, \
             created_at, actor_id, purger_id, purged_at, causation_id, correlation_id, \
             schema_version) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) \
             RETURNING global_sequence",
            self.events_table,
        );
```

Production insert #2 — `store_event` (single-event path, `epoch_pg/src/event_store.rs:454-462`): byte-identical column list (no `global_sequence`), executed via `.fetch_one(&self.postgres)` (autocommit single statement).

Transactions that wrap the inserts:

- `store_events` / `store_events_without_publish` begin and commit their own short transaction:

  ```rust
  // epoch_pg/src/event_store.rs:526-528 (and identically :548-550)
        let mut tx = self.postgres.begin().await?;
        let stored_events = self.store_events_in_tx(&mut tx, events).await?;
        tx.commit().await?;
  ```

- The CQRS command path: `TransactionalAggregate::begin()` opens the transaction from the pool and the aggregate's `store_events_in_tx` runs inside it:

  ```rust
  // epoch_pg/src/aggregate.rs:209-211
                let tx = self.$pool_field.begin().await?;
                Ok(::epoch_core::aggregate::AggregateTransaction::new(
                    self, $crate::aggregate::PgTransaction::new(tx),
  ```

  ```rust
  // epoch_pg/src/aggregate.rs:216-227
            async fn store_events_in_tx(
                &self,
                tx: &mut Self::Transaction,
                events: ::std::vec::Vec<::epoch_core::event::Event<Self::SupersetEvent>>,
            ) -> ... {
                self.$event_store_field
                    .store_events_in_tx(&mut **tx, events)
  ```

  If the command handler fails validation or the commit fails, this transaction rolls back — the nextval() side effects do not.

Rollback-burn attestation, verbatim from the repo (three independent sites):

```text
-- epoch_pg/src/event_bus/mod.rs:4626-4631 (test comment)
// Runs on its own isolated events table (own sequence too): the
// nextval() below burns a slot that is never filled, leaving a permanent
// hole. On the shared epoch_events table that hole would stall any other
// test binary's brand-new, checkpoint-less catch-up/live-loop pass that
// happens to walk over it.
```

```text
-- epoch_pg/tests/pgeventbus_integration_tests.rs:2260-2268 (test doc)
/// Test that a rolled-back transaction creating a permanent gap in global_sequence
/// is resolved by the periodic timer after gap_timeout expires.
///
/// Scenario:
///   1. Event at seq N commits normally
///   2. A transaction obtains seq N+1 via nextval() but rolls back (permanent gap)
```

```text
-- epoch_pg/src/event_bus/mod.rs:3458-3460 (catch-up doc)
/// seen (spec 0026 R1). `global_sequence` is assigned by a non-transactional
/// `nextval()` (spec 0019), so a visible page can contain a hole a still-open
/// transaction fills in later;
```

Also `mod.rs:2369-2372`: "with non-transactional `nextval` (spec 0019) the head may be a burned/in-flight value that never becomes visible, because the transaction that reserved it rolled back or is still open."

Greps confirm there is **no explicit `SELECT nextval` anywhere in the production insert path** — the only `SELECT nextval` hits are test helpers (`mod.rs:4660`, `mod.rs:4735`) and the m002 one-row-at-a-time backfill loop (`m002_add_global_sequence.rs:51-56`).

**Confidence:** verified (code + in-repo attestations; PG's non-transactional nextval is likewise asserted verbatim in four places in the codebase).

**Implications:** Option B (burnless allocation) must replace exactly this DEFAULT-driven per-row `nextval` inside the insert transaction; every writer path (batch tx, single-event autocommit, aggregate tx) funnels through `store_events_in_tx` / `store_event`, so the surface to change is small. The burn is permanent by construction — the "gap fills later" possibility exists only for a *still-open* writer, not for a rolled-back one.

---

## Q2: GapUnproven origin — construction sites and the catch-up path

**Verdict:** `HaltReason::GapUnproven` is constructed at exactly **one** production site: `mod.rs:553`, inside `process_subscriber_for_batch` (`fn` at `mod.rs:300`) — confirming the issue's "live-batch path only" at the construction-site level. Catch-up (`catch_up_from_checkpoint`, `mod.rs:3492`) is fence/backstop-free for gaps. One precision: `process_subscriber_for_batch` is also called by the P4b private fetch, so the same site is reachable from the private path — but only for Checkpointed subscribers; ReplayAlways is excluded from the private fetch (Q4), so for ReplayAlways the shared live batch is the only firing path.

**Method:** Repo-wide grep for `GapUnproven` and `HaltReason::` across all workspace crates and tests; read `process_subscriber_for_batch`'s halt block; read `catch_up_from_checkpoint` in full (3492-3720) and grepped for snapshot/backstop machinery inside it.

**Evidence:**

The single construction site (comment + call), with the function header above it:

```rust
// epoch_pg/src/event_bus/mod.rs:544-556
    // Spec 0028 P4 gap refusal: fire on_halt(GapUnproven) on the first batch
    // cycle where the backstop would have fired but was refused. Subsequent
    // cycles for the same gap return None (halt_fired guards the entry-only
    // contract, spec 0028 §3.4).
    if let Some(refused_seq) = outcome.backstop_refused {
        fire_on_halt(
            &config,
            &subscriber_id,
            refused_seq,
            HaltReason::GapUnproven,
        )
        .await;
    }
```

The variant definition (`epoch_pg/src/event_bus/config.rs:80-85`):

```rust
    /// The gap-timeout backstop would have advanced past a missing sequence,
    /// but the subscriber is [`FailClosed`]. The checkpoint is held below the
    /// gap until the writer's transaction is proven absent (fence clears
    /// because the writer aborted) or an operator releases the subscriber.
    ...
    GapUnproven,
```

Every `HaltReason::…` construction in `epoch_pg/src` (grep): `DeserializeFailure` at `mod.rs:420, 3608, 4134`; `ObserverFailure` at `mod.rs:513, 3684, 4205`; `Released` at `mod.rs:2348`; **`GapUnproven` at `mod.rs:553` only**. Tests reference `HaltReason::GapUnproven` solely as assertions against fired reasons (`epoch_pg/tests/pgeventbus_integration_tests.rs:8536, 8828`) — not firing sites.

Catch-up is fence/backstop-free — the function's own doc (`mod.rs:3457-3462`):

```text
/// Recovering a *permanent* hole
/// (one that will never fill) is the live listener's job, not this pass's:
/// catch-up has no gap fence, snapshot, or timeout backstop of its own (see
/// spec 0026 §3).
```

Corroborated mechanically: `advance_contiguous_checkpoint` (the gap fence/backstop resolver) has exactly one production call site — `mod.rs:536`, inside `process_subscriber_for_batch` (all other hits are unit tests in `subscriber_state.rs`). `query_txid_snapshot` is called only at `mod.rs:895` (private fetch) and `mod.rs:2041` (shared live batch) — never in `catch_up_from_checkpoint` (3492-3720), whose only position logic is `advance_catchup_prefix` (calls at `mod.rs:3632-3643` and `3677-3687`) plus fail-closed per-event halts (`DeserializeFailure` `mod.rs:3608`, `ObserverFailure` `mod.rs:3684` — not gap halts).

The P4b reachability nuance (`mod.rs:920-928`):

```rust
    process_subscriber_for_batch(
        projection,
        subscriber_id,
        state,
        pending_checkpoint,
        last_event_id,
        ctx,
    )
    .await
```

**Confidence:** verified.

**Implications:** Any new gap policy (option A) has a single natural choke point — the `outcome` handling in `process_subscriber_for_batch` — but it will take effect on both the shared and private-fetch routes unless gated by subscription mode, as P4b's `replay_always_by_sid` filter does.

---

## Q3: Unbroken-prefix HWM guard (CLOUD-227) — what happens to a page containing a hole?

**Verdict:** With the guard, the ReplayAlways HWM (and catch-up contiguous prefix) freezes at `hole − 1`; events above the hole are still dispatched and counted but never advance the HWM while the hole stands; catch-up still terminates because the pagination cursor advances by max-seq-seen. Nothing ever advances the HWM past an unproven hole afterwards — catch-up re-seeds from the pinned HWM, `head_sequence` is read-only, and the only HWM writes are the prefix-advance sites plus the `subscribe()` reset to 0.

**Method:** Read `advance_catchup_prefix` (3393-3433), the live-batch HWM routing in `process_subscriber_for_batch` (~700-712), the catch-up loop body (3530-3720), and grepped every `hwm` touch point in `mod.rs`.

**Evidence:**

The guard, exactly at the issue's cited anchor (`mod.rs:3410-3425`, inside `advance_catchup_prefix`):

```rust
// epoch_pg/src/event_bus/mod.rs:3410-3425
    if replay_always {
        // CLOUD-227: apply the same contiguous-prefix guard as Checkpointed.
        // Before this fix, the HWM was always the maximum sequence seen,
        // meaning a hole in the catch-up window would cause the live listener
        // to re-seed `contiguous_checkpoint` above the hole, permanently
        // skipping the missing sequence. Now the HWM advances only across an
        // unbroken prefix, matching the Checkpointed path exactly.
        if event_global_seq != *contiguous + 1 {
            return;
        }
        *contiguous = event_global_seq;
        hwm.lock()
            .await
            .insert(subscriber_id.to_string(), event_global_seq);
        return;
    }
```

Git provenance: `git log --grep="CLOUD-227"` → `8bebf89 fix(pg): ReplayAlways HWM advances only across the contiguous prefix`.

Trace of a catch-up page containing a hole at seq G (subscribers at prefix G−1):

1. Rows above the hole **are dispatched** to the observer — `process_event_with_retry` runs for every row (`mod.rs:3659-3660`), then:
2. `advance_catchup_prefix` returns early for each such row (`event_global_seq != *contiguous + 1` → `return`, `mod.rs:3418-3420`) — HWM and `contiguous` do **not** move;
3. The pagination cursor **does** move — `current_sequence = event_global_seq;` (`mod.rs:3685`), so the pass reads on past the hole and terminates at head ("The cursor is the highest `global_sequence` reached…", `mod.rs:3475-3478`; "the pass still terminates because the pagination cursor keeps advancing by the maximum sequence seen (§4.1, spec 0026 R3)", `mod.rs:3466-3469`).

Live-batch path: events above the hole in the shared window are likewise delivered and recorded via `record_applied!` → `state.processed_ahead` (`mod.rs:352-364`, `501-503`), but the HWM is only written when the *contiguous prefix* advanced (`mod.rs:700-712`):

```rust
// epoch_pg/src/event_bus/mod.rs:700-712
    let new_contiguous = state.contiguous_checkpoint;
    ...
    if new_contiguous > contiguous_before {
        ...
        if replay_always {
            // Route advancement to the in-memory HWM; never touch the
            // checkpoints table for a ReplayAlways subscriber.
            hwm.lock()
                .await
                .insert(subscriber_id.clone(), new_contiguous);
```

Does anything advance the HWM past the hole later? Exhaustive list of HWM writes (grep over `mod.rs`): `mod.rs:712` (live prefix advance), `mod.rs:3421` (catch-up prefix advance), `mod.rs:3920` (`subscribe()` reset to 0), `mod.rs:4876` (unit test). The prefix advance in the live path requires `advance_contiguous_checkpoint` to move past the gap, which for a fail-closed subscriber happens only via **FenceCleared** (proven permanent) — never via the timeout backstop (Q5). Catch-up re-seeds from the same pinned HWM (`mod.rs:3503-3508`), so it can never jump the hole either. `head_sequence` (`mod.rs:2368+`) is a readiness/lag query only; it writes nothing.

**Confidence:** verified.

**Implications:** This is the mechanism behind the issue's claim (2): under a live ReplayAlways + fail-closed subscriber with an unproven gap, the HWM pins below the hole "forever" in the sense that no code path advances it — the hole must either fill (writer commits) or be proven permanent (fence) before the prefix — and hence the HWM — can move again.

---

## Q4: P4b self-heal — exclusion, coverage, and `release_halt`

**Verdict:** P4b private-fetch self-heal covers wedged fail-closed **Checkpointed** subscribers only; ReplayAlways wedges are explicitly out of scope by comment and by filter. `release_halt` is defined at `mod.rs:2290`, advances the *persisted checkpoint row* forward and fires `HaltReason::Released`; it is a public operator API — epoch's own code never calls it (only integration tests do).

**Method:** Read `mod.rs:1871-1935` (P4b pass), `subscriber_state.rs:120-215` (wedge predicate, release adoption), `mod.rs:2258-2350` (`release_halt`), and grepped `release_halt` across the workspace including tests.

**Evidence:**

The exclusion, verbatim (`mod.rs:1871-1893`; the issue's cited anchor 1884-1893 holds):

```text
-- epoch_pg/src/event_bus/mod.rs:1871-1893
                // === Private fetch pass for wedged subscribers (spec 0028 P4b) ===
                //
                // A wedged (halted) fail-closed `Checkpointed` subscriber is
                // excluded from the shared `min_checkpoint` floor so it never
                // starves healthy peers (R13). It is instead driven once per wake
                // by a private fetch from its OWN persisted checkpoint, which also
                // lets an operator release (R14) take effect on a running bus and
                // lets a gap wedge self-heal via `FenceCleared` from the private
                // path. The shared-row break conditions do not apply here: this is
                // a single pass over every wedged subscriber, after which the
                // cycle continues into the shared loop (or, when everyone is
                // wedged, ends there and the timer tick re-enters).
                //
                // `ReplayAlways` wedges are out of P4b's scope (no persisted
                // checkpoint row to re-seed from; the remedy is a fresh
                // `subscribe()`, R9b) — the `ReplayAlways` floor-exclusion analogue
                // lands in P5. This is the documented, bounded intra-pipeline gap.
                let wedged_sids: Vec<String> = subscriber_states
                    .iter()
                    .filter(|(sid, s)| {
                        s.is_wedged() && !replay_always_by_sid.get(*sid).copied().unwrap_or(false)
                    })
                    .map(|(sid, _)| sid.clone())
                    .collect();
```

What "wedged" means (`subscriber_state.rs:174-183`):

```rust
    /// A wedge is either a held event ([`held_event`](Self::held_event), set on
    /// the deserialize- and observer-exhaustion halt paths) or a **refused** gap
    /// (a `TimeoutBackstop` the fail-closed policy declined to skip, marked by
    /// `GapObservation::halt_fired`). A brand-new, not-yet-refused gap does NOT
    /// count: it may still fill or clear normally, exactly as it would for a
    /// fail-open subscriber (R10 — fail-open subscribers are never wedged).
    pub(crate) fn is_wedged(&self) -> bool {
        matches!(self.failure_mode, FailureMode::FailClosed)
            && (self.held_event.is_some() || self.gap_first_seen.values().any(|obs| obs.halt_fired))
    }
```

So P4b covers: fail-closed subscribers with either (a) a held event (deser/observer halt) or (b) a refused gap — provided they are **not** ReplayAlways. Its self-heal mechanisms, verbatim:

- FenceClear self-heal (`mod.rs:890-894`): "Own txid snapshot, under the SAME gate the shared path uses (`snapshot_fencing` && a gap is active), so a gap-wedged subscriber's `FenceCleared` recovery can still fire from the private path."
- Held-event re-attempt (`mod.rs:466-470`): "A fail-closed subscriber that halted on exactly this sequence in an earlier cycle re-attempts it with a SINGLE observer invocation and no retry ladder (spec 0028 §3.4)." Success clears `held_event` (`mod.rs:477`).
- Operator-release reconciliation (`mod.rs:816-831` re-reads the persisted checkpoint and calls `state.adopt_released_cursor(...)` when it moved forward; `subscriber_state.rs:185-206`).

`release_halt` (`mod.rs:2290`; doc `mod.rs:2261-2263`: "Releases a wedged (halted) fail-closed `Checkpointed` subscriber by advancing its persisted checkpoint *past* a held sequence it never finished (spec 0028 §3.4 / R14)"). What it does, from its body:

1. Forward-only guard — `BackwardRelease` error if `past_sequence <= current` (`mod.rs:2297-2303`);
2. Upserts the checkpoint row (`mod.rs:2317-2331`):

   ```sql
            INSERT INTO epoch_event_bus_checkpoints (bus_name, subscriber_id, last_global_sequence, last_event_id, updated_at)
            VALUES ($1, $2, $3, $4, NOW())
            ON CONFLICT (bus_name, subscriber_id) DO UPDATE SET
                last_global_sequence = EXCLUDED.last_global_sequence, ...
   ```

3. `warn!` operator log (`mod.rs:2333-2339`);
4. Fires the halt callback with `HaltReason::Released` (`mod.rs:2341-2345`).

Call sites (repo-wide grep, including tests): **production code never calls it.** The only callers are integration tests — `epoch_pg/tests/pgeventbus_integration_tests.rs:8836` ("operator `release_halt` past the gap resumes it in-process without a restart"), `:8963`, `:8974`, `:9473`. All other hits are doc references (`config.rs:87, 92, 337, 341`; `mod.rs:1019, 1025, 2261`; `epoch_core/src/event_store.rs:272`).

Note for ReplayAlways: even if an operator called `release_halt` for a wedged ReplayAlways subscriber, it writes the checkpoint table — which every ReplayAlways position path ignores (`position_for_mode` reads the HWM, `mod.rs:2413-2418`; catch-up seed `mod.rs:3503-3508`; live seeding `mod.rs:1833-1837`) — and the doc scopes it to Checkpointed (`mod.rs:2261`). It would not un-wedge the in-memory state.

**Confidence:** verified.

**Implications:** The issue's premise (3) is exactly what the code says: for ReplayAlways there is no private-fetch self-heal and no effective release API; the code's own designated remedy is a fresh `subscribe()`.

---

## Q5: Fence + backstop + halt chain — exact runtime sequence and persisted state

**Verdict:** Verified end-to-end. Fence test is `snap.xmin >= fence_xmax` (`subscriber_state.rs:349-350`, inside the issue's cited 337-363 block). `gap_timeout` field at `config.rs:307`; default `Duration::from_secs(5)` at `config.rs:388` (both cited anchors exact). Sequence: gap armed on first observation → fence evaluated per batch → after 5 s the FailClosed arm "refuses" the backstop (no skip, no advance, `backstop_refused = Some(next)` on first entry) → caller fires `on_halt(GapUnproven)` → subscriber becomes wedged and is excluded from every fetch path. **Nothing is persisted by epoch on this path** — no halt flag on disk, no `epoch_event_bus_gap_timeouts` row (that table is written only for fail-open TimeoutBackstop skips), no DLQ row, and (for ReplayAlways) no checkpoint row.

**Method:** Read `advance_contiguous_checkpoint` in full (`subscriber_state.rs:306-437`), the caller's outcome handling (`mod.rs:536-660`), `fire_on_halt` (`mod.rs:271-297`), the config field/default, the m009 schema, and the wedge-exclusion sites.

**Evidence:**

Fence logic (gap branch, verbatim; issue's cited range 337-363 holds — the fence condition itself sits at 349-350):

```rust
// epoch_pg/src/event_bus/subscriber_state.rs:331-350 (inside advance_contiguous_checkpoint)
        if has_events_above && !visible_seqs.contains(&next) {
            // `next` is not visible in the DB and not processed — this is a gap.
            // Either an uncommitted transaction or a rolled-back one.
            if let Some(observation) = state.gap_first_seen.get_mut(&next) {
                // LAZY BACKFILL: if we had no snapshot at first observation, capture
                // a fence boundary now so this gap can be fenced going forward.
                if observation.fence_xmax.is_none()
                    && let Some(snap) = snapshot
                {
                    observation.fence_xmax = Some(snap.xmax);
                }
                ...
                // FENCE (fast path): can we PROVE the gap is permanent? Every txn that
                // was in-flight at detection has finished (xmin >= fence_xmax) and the
                // sequence is still missing, so its writer aborted (or it was burned).
                if let (Some(snap), Some(fence)) = (snapshot, fence_xmax)
                    && snap.xmin >= fence
                {
```

Fence semantics doc (`subscriber_state.rs:283-287`): "a gap is only skipped once the fence proves it permanent (`xmin >= fence_xmax`, where `fence_xmax` is the `xmax` captured when the gap was first observed) — meaning every transaction that was in-flight at detection has completed and the sequence is still missing."

`gap_timeout` field and default:

```rust
// epoch_pg/src/event_bus/config.rs:295-307
    /// Maximum time to wait for a sequence gap to fill before assuming the
    /// transaction was rolled back.
    /// ...
    /// Default: 5 seconds
    ...
    pub gap_timeout: Duration,
```

```rust
// epoch_pg/src/event_bus/config.rs:381-391 (impl Default for ReliableDeliveryConfig)
            catch_up_batch_size: 100,
            catch_up_buffer_size: 10_000,
            gap_timeout: Duration::from_secs(5),
```

Runtime sequence, with functions and state transitions:

1. **Live batch observes gap.** Shared fetch reads rows above the non-wedged floor (`mod.rs:1966-1975`); `process_subscriber_for_batch` (`mod.rs:300`) delivers each visible event; `advance_contiguous_checkpoint` (`mod.rs:536` → `subscriber_state.rs:306`) finds `next = contiguous + 1` missing while `has_events_above` (`subscriber_state.rs:328-331`).
2. **Fence armed.** First observation inserts `GapObservation { first_seen: Instant::now(), fence_xmax: snapshot.map(|s| s.xmax), halt_fired: false }` (`subscriber_state.rs:410-417`). Note the OQ-4 gate: the snapshot is queried only when some gap is already active (`mod.rs:2033-2047`), so a brand-new gap usually arms with `fence_xmax = None` and the boundary is lazily backfilled on the next batch (`subscriber_state.rs:335-341`) once `query_txid_snapshot` runs (`mod.rs:2041`).
3. **Fence evaluated each batch.** If `snap.xmin >= fence_xmax` → `SkipReason::FenceCleared`, gap removed, `contiguous_checkpoint = next` (`subscriber_state.rs:351-367`) — this advance happens under **both** failure modes ("The fence-clear branch is untouched: an event proven never to have existed is advanced past under both modes", `subscriber_state.rs:299-302`), and routes to the ReplayAlways HWM at `mod.rs:712`.
4. **Backstop fires after timeout — and refuses.** If the fence is not cleared and `gap_duration > gap_timeout` (`subscriber_state.rs:374-375`), the `FailClosed` arm (`subscriber_state.rs:376-384`) is the concrete meaning of "the backstop refuses":

   ```rust
   // epoch_pg/src/event_bus/subscriber_state.rs:376-384
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
   ```

   i.e.: no `SkippedGap` is pushed, the gap stays in `gap_first_seen`, `contiguous_checkpoint` is **not** advanced, and `backstop_refused = Some(next)` is returned on the first entry only. (Fail-open instead pushes `SkipReason::TimeoutBackstop` and advances — `subscriber_state.rs:385-403`.)
5. **GapUnproven.** The caller fires the halt once (`mod.rs:549-556`, quoted in Q2). `fire_on_halt` (`mod.rs:271-297`) only invokes the user-supplied `config.on_halt` callback with `HaltInfo { subscriber_id, held_below_sequence, reason }`, containing panics — it persists nothing.
6. **Halt/wedge.** With `gap_first_seen[..].halt_fired == true`, `is_wedged()` becomes true (`subscriber_state.rs:180-183`). Consequences, all in-process:
   - excluded from the shared floor: `compute_shared_floor` skips wedged states (`subscriber_state.rs:228-247`); if *all* subscribers are wedged the shared fetch is skipped (`mod.rs:1958-1963`);
   - excluded from the shared batch dispatch: `wedged_now` filter (`mod.rs:2014-2022`) applied at `mod.rs:2074` ("Wedged subscribers are driven by the private pass, never the shared batch (spec 0028 P4b): they are excluded here so they see their own `visible_seqs`…");
   - driven by the P4b private fetch — **Checkpointed only** (Q4); ReplayAlways gets nothing.

What is persisted, exactly:

- **Halt flag:** none on disk. `halt_fired` is a field of the in-memory `GapObservation` (`subscriber_state.rs:75-79`), held in the listener task's `subscriber_states` map ("All wedge bookkeeping (contiguous position, `processed_ahead`, `gap_first_seen` including the first-captured `fence_xmax`, and `held_event`) is preserved across cycles" — `mod.rs:792-798`, i.e., across *batch cycles*, not process restarts).
- **`epoch_event_bus_gap_timeouts` row:** **not written for a refused gap.** The row is inserted only for fail-open `TimeoutBackstop` skips (spawned fire-and-forget INSERT at `mod.rs:644-660`; schema m009 `epoch_pg/src/migrations/m009_create_gap_timeout_log.rs:30-44`: "Records each global sequence that a subscriber's checkpoint was advanced past because the gap did not fill within `gap_timeout`"). A fail-closed refusal never produces a `SkippedGap`, so the partition at `mod.rs:559-566` puts it in neither `fence_cleared` nor `timeout_backstop`. The batched WARN naming that table (`mod.rs:594-601`) is likewise fail-open-only.
- **Checkpoint row:** ReplayAlways never writes one (HWM routing at `mod.rs:706-712`; "the checkpoints table, which is never written for such a subscriber", `mod.rs:3489-3490`).
- **DLQ row:** none — no event failed; the gap was merely observed.

**Confidence:** verified.

**Implications:** The issue's claim (1) is confirmed by construction: the live batch arms the fence, the backstop refuses at 5 s for a fail-closed subscriber when the fence stays unproven, `GapUnproven` fires from the live-batch path, and the subscriber halts — with zero durable trace in epoch's own tables. The only durable artifact of the whole episode is the hole itself in `epoch_events` (absence of a row).

---

## Q6: Wedge persistence and recovery across restart

**Verdict:** A ReplayAlways GapUnproven wedge leaves **no persisted epoch state** (its position, the HWM, is "Never persisted"; the halt flag is in-memory; no gap-timeout or checkpoint or DLQ row exists). On process restart nothing re-reads any halt state — there is none to read; the application must re-register the subscriber (`subscribe()`), which zeroes the HWM and replays from 0, re-detects the gap, and re-wedges after ~`gap_timeout` (5 s) unless the fence can now prove the gap permanent. In-process auto-recovery exists for Checkpointed wedges (private-fetch FenceCleared, held-event re-attempt, release reconciliation) and **not** for a ReplayAlways gap wedge — the code's own stated remedy is a fresh `subscribe()`.

**Method:** Read the HWM field doc, `SubscriberState` constructors, the listener's per-wake seeding block, `subscribe()`'s HWM reset, the checkpoints-table schema, the gap-timeouts readers, and all wedge-exclusion/self-heal sites.

**Evidence:**

The HWM is never persisted (`mod.rs:1175-1179`, field doc):

```text
    /// Per-subscriber in-memory high-water mark for [`SubscriptionMode::ReplayAlways`]
    /// subscribers. Never persisted: a crash loses it and the next boot replays
    /// from 0, which is the intended contract. Shared across `Clone`s so
    /// `subscribe`, the listener task, and readiness queries observe the same value.
```

ReplayAlways never writes the checkpoints table (`mod.rs:706-712`, quoted in Q3; and `mod.rs:3483-3490`: "it ignores the persisted checkpoint entirely… routing advancement to that HWM instead of the checkpoints table, which is never written for such a subscriber").

Halt state is rebuilt empty on every seed — `SubscriberState::new` / `new_with_event_id` start with `gap_first_seen: HashMap::new()` and `held_event: None` (`subscriber_state.rs:139-167`), and the listener seeds fresh state for any subscriber id not already in its map (`mod.rs:1817-1845`; for ReplayAlways it seeds from the HWM, `mod.rs:1832-1837`). The persisted checkpoints table has no halt column at all (m003 `epoch_pg/src/migrations/m003_create_event_bus_infrastructure.rs:63-66`: columns are `last_global_sequence BIGINT NOT NULL DEFAULT 0, last_event_id UUID`). `epoch_event_bus_gap_timeouts` is read only by the operator listing API `list_gap_timeouts` (`mod.rs:2908+`, queries at `2941-2998`); no listener path reads it to re-arm anything.

Does the subscriber resume delivering on restart? Process restart:

- The projection registry is in-memory (`projections: Arc<Mutex<Vec>>` created in `with_config`, `mod.rs:1203`), so the application must call `subscribe()` again as a matter of course.
- `subscribe()` resets the ReplayAlways HWM to 0 before catch-up (`mod.rs:3914-3920`):

  ```rust
            // A fresh subscribe of a ReplayAlways subscriber rebuilds its in-memory
            // model from empty, so reset the HWM before catch-up: ...
            if replay_always {
                hwm.lock().await.insert(subscriber_id.clone(), 0);
            }
  ```

- Catch-up then replays from 0 and pins below the permanent hole (Q3's guard); the live loop seeds from the pinned HWM, re-detects the gap, re-arms the fence, and — if the fence still cannot prove within 5 s — the backstop refuses and `GapUnproven` fires again. The wedge therefore **re-forms after every restart with a fresh `gap_timeout` delay**, unless the fence can now prove (e.g., the previously pinning in-flight transaction is gone and the sequence is still missing → `FenceCleared` advance in the live path, `subscriber_state.rs:349-367`).

Any auto-recovery path for a halted subscriber?

- **Checkpointed, fail-closed:** yes, three — (a) P4b private fetch re-fetches from its own checkpoint with its own snapshot so "a gap-wedged subscriber's `FenceCleared` recovery can still fire from the private path" (`mod.rs:890-894`); (b) held events are re-attempted once per cycle and a success clears the hold (`mod.rs:466-484`); (c) an operator `release_halt` is reconciled in-process via `adopt_released_cursor` (`mod.rs:816-831`).
- **ReplayAlways, gap wedge:** **no in-process auto-recovery.** Once `halt_fired` is set it is excluded from the shared batch (`mod.rs:2014-2022`, `2074`) and from the private fetch (`mod.rs:1888-1893`), so `advance_contiguous_checkpoint` is never invoked for it again — the fence cannot even be re-evaluated. The code's own remedy comment: "the remedy is a fresh `subscribe()`, R9b" (`mod.rs:1884-1887`). A fresh `subscribe()` (or a restart) re-enters the cycle described above. `release_halt` is documented Checkpointed-only (`mod.rs:2261`) and, as noted in Q4, writes a table the ReplayAlways pipeline never reads.

**Confidence:** verified (all claims are direct code reads; the "re-wedge after restart" composite is a desk-trace of the cited paths, each individually verified).

**Implications:** The wedge is not a persisted deadlock but a **self-re-establishing cycle**: each boot pays a full replay from 0 plus ~5 s of gap_timeout before re-halting — while, the whole time, events continue to be delivered above the hole and then lost again on the next rebuild (ReplayAlways semantics). For option A, the absence of any durable skipped-seq record (Q5) means a SkipAfterBackstop policy must introduce its own persistence; there is nothing to piggyback on.

---

## Notes (observations relevant to option A safety — listed, not chased)

1. **What the fence proves — and about whom.** The fence proves only that "every transaction that was in-flight **at detection** has completed and the sequence is still missing" (`subscriber_state.rs:284-287`). It is evaluated against the detection-time in-flight set (`fence_xmax = snapshot.xmax` at first observation, lazily backfilled). While the gap's own writer is still running, `xmin` stays below `fence_xmax`, so the fence is pinned and cannot distinguish "writer will abort" from "writer will commit late" — that is precisely the "unproven" in `GapUnproven`. A SkipAfterBackstop decision made at backstop time is therefore made while the fence is, by definition, still pinned — i.e., exactly while an in-flight writer *may* still commit the skipped seq.
2. **A late commit at a skipped seq is invisible to the subscriber.** All reads are strictly `global_sequence > <position>` (catch-up query `mod.rs:3514-3522`; live shared fetch `mod.rs:1966-1975`). Once the position advances past a skipped seq, a row committing at that seq is never re-read — no detection mechanism for late-materializing rows exists anywhere in the crate. Option A's "detect and trigger a rebuild" would be net-new machinery (e.g., a durable skipped-seq ledger plus a scan, or a DB-side tripwire).
3. **No durable skipped-seq ledger exists today, in either skip path.** Fail-open `TimeoutBackstop` skips persist to `epoch_event_bus_gap_timeouts` (`mod.rs:644-660`); fail-closed **refused** gaps persist nothing (Q5); fail-closed **FenceCleared** skips persist nothing either — "Fence-cleared gaps are proven permanent with no data loss: a single debug line, no WARN / record / callback" (`mod.rs:578-583`). If option A wants "track skipped seqs", the ledger must be written on the new skip path — and note the tracking table's semantics are currently "checkpoint was advanced past", which a fail-closed refusal never satisfies.
4. **ReplayAlways positions are ephemeral by design.** The HWM is never persisted and is zeroed by `subscribe()` (`mod.rs:1175-1179`, `3914-3920`). Consequences for option A: (a) a SkipAfterBackstop HWM advance is inherently in-memory and dies with the listener; (b) the next fresh rebuild replays from 0 and *will* encounter a late-committed row at the skipped seq (the hole is filled by then), so the rebuild self-heals — the exposure window is the lifetime of the live in-memory model, not forever.
5. **Fence-cleared skips are already lossless for burns.** For a genuinely rolled-back writer with fencing enabled and no pinning transactions, the fence clears within a batch cycle or two and *both* failure modes advance (`subscriber_state.rs:299-302`). The wedge requires the fence to stay unproven past 5 s: fencing disabled (`snapshot_fencing: false` → `snapshot = None`, `config.rs:357-377`), a snapshot-query failure, or a long-lived transaction started before detection still running at timeout. Option A's SkipAfterBackstop only bites in those cases — but case "long-lived writer" is exactly the in-flight-writer risk the issue flags.
6. **The gap-timeout WARN text overstates for fail-closed readers.** The batched WARN says the gap is "recorded in epoch_event_bus_gap_timeouts" (`mod.rs:594-601`), but that record path executes only for fail-open skips; a fail-closed GapUnproven leaves no row. Anyone building option A tooling on that table should not assume refused-gap rows exist.
7. **Not chased:** whether the P5 "ReplayAlways floor-exclusion analogue" (`mod.rs:1886-1887`) has since landed elsewhere; `compute_shared_floor` already excludes *all* wedged states regardless of mode (`subscriber_state.rs:228-247`), so the comment's precise P5 scope was not pursued.

---

## Prober discipline attestation

- Mode honored: paper test — no repository mutation, no cargo invocation; all evidence from reading files at HEAD `bed756b` and `git log`.
- Residue: this report file only (`docs/probes/cloud261-failure-chain-paper-20260915.md`, newly created; `docs/probes/` did not exist before).
- Negative results reported as such: no `SELECT nextval` in production insert paths (Q1); no persisted halt state anywhere (Q6); no auto-recovery path for a ReplayAlways gap wedge (Q6); no detection mechanism for late-materializing rows (Notes 2).
