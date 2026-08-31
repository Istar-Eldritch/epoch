# Delivery Plan: Spec 0028 — Per-Subscription Fail-Closed Delivery Semantics

**Spec:** `specs/0028-cloud216-fail-closed-subscriber-semantics.md` (approved) · **Status:** Awaiting plan approval (round 2 — fixes applied after a 3-reviewer round found 1 P0 and several P1/P2s; anchors re-verified at `a19d001`) · **Method:** TDD — every phase lands failing test(s) first, then implementation, then refactor · **Commits:** one Conventional Commit per phase, made after review of that phase.

**Standing rules for every phase:** `cargo fmt` + `cargo clippy -- -D warnings` clean; no
`unwrap()`/`expect()` outside `#[cfg(test)]`/`tests/`; relative sequence discipline only
(`INSERT … RETURNING global_sequence` via `insert_committed_event`); every fail-closed test runs
either on `isolated_events_table` or under `#[serial]` with `setup()` — and if it uses the shared
table, the fail-closed subscriber's starting checkpoint must be pre-planted above any historical
row (`setup()` truncates on every call, tests/common/mod.rs:249-258, but a subscriber that starts
at checkpoint 0 catches up over the *whole* table, and a leftover malformed row elsewhere in the
suite would wedge it before the row under test — tests:2746-2753 is one such row). T9's wedge
variants need this explicitly (see T9 below).
**Re-anchor duty:** all `mod.rs`/line anchors below were verified at `a19d001`. Before editing a
site, re-grep the anchor by symbol, not by raw line number — every phase after P1 shifts lines to
some degree (P1's own `epoch_pg` footprint, once P2 adds it, lands downstream of most later sites,
so the drift risk runs in both directions depending on phase order).

## Test utilities (built with the phase that first needs them)

| Utility | Home | Shape | First needed |
|---|---|---|---|
| `ForwardingProjection` | `epoch_core` (`#[cfg(test)] mod tests` at `projection.rs:266` / `saga.rs:464`, or `epoch_core/tests/`) | Test `Projection` whose `failure_mode()` is configurable (overrides the default) | P1 (T0) — **cannot** live in `epoch_pg/tests/`; T0 is an `epoch_core`-only unit test |
| `FailingObserver<D>` | `epoch_pg/tests/` | `EventObserver` whose `on_event` always returns `Err` | P2 (T3) |
| `PanickingObserver<D>` | `epoch_pg/tests/` | `on_event` panics (deterministic message) | P2 (T8) |
| `CapturingDlqCallback` / `CapturingHaltCallback` | `epoch_pg/tests/` | `Arc<Mutex<Vec<DlqInsertionInfo/HaltInfo>>>` capture, mirror existing callback-capture patterns (config.rs examples) | P2 (T1/T3) |
| `read_dlq_rows(pool, subscriber_id)` | `epoch_pg/tests/` | Raw SQL `SELECT` against `epoch_event_bus_dlq` (pattern at tests:1014/1046) — **`list_dlq_entries` does not exist**; do not assume a bus-level DLQ-read API | P2 (T1/T3) |
| `fix_event_payload(pool, table, seq, data)` | `epoch_pg/tests/` | `UPDATE <table> SET data = $1 WHERE global_sequence = $2` | P2 (T2) |
| `count_invocations` wrapper | `epoch_pg/tests/` | Observer wrapper counting `on_event` calls per sequence, exposed for polling (not a single post-hoc read) | P2 (T3's invocation-bound assertion) |

## P1 — Trait surface (`epoch_core` only)

Scope note: this phase touches **only `epoch_core`**. The original draft also tried to land the
`epoch_pg` resolution plumbing here via a standalone `failure_modes: HashMap<...>` mirroring
`subscriber_modes` — that target does not exist on the delivery path (`subscriber_modes`,
mod.rs:820, is a readiness-only registry: written only by `warn_if_subscriber_id_reused`,
mod.rs:2777-2794, called at 3162/3236; read only at 1838/2067/2781; never cloned into the spawned
listener task, mod.rs:1166-1172; and has no removal path anywhere, including `shutdown()`,
mod.rs:1702-1725). Landing an unconsumed `pub(crate)` reader in this phase would also fail
`clippy -- -D warnings` as dead code (`epoch_pg/src/lib.rs:42` has `#![deny(missing_docs)]`, no
`allow(dead_code)`). The resolution plumbing moves to P2, where it lands with its first consumer.

1. **Failing test T0** (`epoch_core` unit test, no bus): a `ForwardingProjection` with
   `failure_mode() -> FailClosed` wrapped in `ProjectionHandler` exposes `FailClosed` through
   `EventObserver::failure_mode()`; same assertions for `SagaHandler`, the `SagaAdapter`, and the
   `Arc<S>` blanket (saga.rs:226-232). Also asserts `FailOpen` defaults on all four traits.
2. **Implement:** `#[non_exhaustive] pub enum FailureMode { FailOpen, FailClosed }` in
   `epoch_core/src/event_store.rs` beside `SubscriptionMode` (:196/:258), with rustdoc carrying
   the R12 frozen-read-model consequence; default `fn failure_mode()` on `EventObserver`,
   `Projection`, `Saga`; forwarding impls in `ProjectionHandler` (its `impl EventObserver`,
   projection.rs:247), `SagaHandler` (:279), `SagaAdapter` (:417), `Arc<S>` blanket (saga.rs:226-232).
   Mirror the top-level convenience re-export pattern already used for `SubscriptionMode`
   (`pub use event_store::SubscriptionMode` at `epoch_core/src/lib.rs:39`) with `FailureMode` —
   the preludes themselves are glob re-exports (`epoch_core/src/lib.rs:43-56`,
   `epoch/src/lib.rs:23-25`), so "export through the prelude" is a no-op; the actual API surface
   addition is the top-level re-export line.
3. **Validate:** T0 green; `epoch_core` compiles with every default body in place (breaks no
   implementor); `epoch_pg` is untouched by this phase — full workspace still compiles and its
   existing suite is unaffected. **Exit:** `FailureMode` exists and forwards correctly through
   `Projection`/`Saga`/handlers/the `Arc` blanket, proven by T0. No `epoch_pg` behavior change;
   `subscribe()`-level resolution is not yet wired (that is P2's first step, and is behaviorally
   proven there by T1's halt actually firing for a `FailClosed` projection — not by a second unit
   test of the resolution site itself).
   Commit: `feat(core): …`.

## P2 — Live-path hold, observability, panic containment (`epoch_pg`)

1. **Failing tests:** T1 (live deser halt: FC checkpoint held at N, later events not applied,
   healthy peer unaffected, `wait_until_caught_up(FC)` → `Ok(false)` — the R8 readiness assertion
   lives here — deser DLQ row (via `read_dlq_rows`) + `on_halt` fired); T2 (live auto-recovery:
   `fix_event_payload(N+1)` after T1's halt, next batch applies N+1/N+2 exactly once, in order);
   T3 (observer-failure halt: DLQ row + `on_dlq_insertion` + `on_halt`, held checkpoint, **poll
   `count_invocations` and assert a bound, not an exact count** — cycles are driven by the 1s
   `flush_interval` (mod.rs:1271, select arm 1341) plus every `NOTIFY`, so exact cycle count is
   not knowable from the test: after waiting N seconds, `count <= first_failure_count +
   ceil(N / 1s) + slack` **and** `count < first_failure_count * max_retries` (proves the ladder is
   not re-run every cycle) — then fix observer → recovery); T8-live (`PanickingObserver`: listener
   task alive; FC → halt + DLQ + `on_halt`; FO → DLQ + continue); T6 (Batched + held wedge: no
   flush across several `max_delay_ms` periods, resumes after fix).
2. **Implement:**
   - **`FailureMode` resolution and threading** (moved here from P1, per the P1 scope note): add
     a `failure_mode: FailureMode` field to `SubscriberState`, captured at state-init
     (mod.rs:1428-1434) next to where `replay_always` is already captured off the same observer
     lock. `process_subscriber_for_batch` reads it off the `state` it already owns as a
     parameter — it does **not** re-lock the observer for this (the function already re-locks
     the observer once, at mod.rs:245-247, only to read `subscription_mode()`; do not add a
     second purpose to that lock or a second lock acquisition). This also makes the value visible
     at the *listener-loop* level for P4b's floor computation (mod.rs:1509-1513), which is
     outside `process_subscriber_for_batch` and needs the field on `SubscriberState` for exactly
     that reason.
   - Deser branch (mod.rs:280-293): `FailClosed` → WARN, insert deser DLQ row
     (`error_message = "unrecoverable: deserialize: <err>"`, `ON CONFLICT (subscriber_id,
     event_id) DO UPDATE`), fire `on_halt(HaltInfo { subscriber_id, held_below_sequence,
     reason: DeserializeFailure })`, set `state.held_event = Some(event_seq)` — this marker is
     the general halted-at marker for **both** deser and observer wedges (see next bullet) —
     `break` the row loop: no `processed_ahead` insert, no `record_processed`, contiguous stays
     below the bad sequence.
   - Observer branch (mod.rs:318-331): on `ProcessResult::SentToDlq` + `FailClosed` → fire
     `on_halt(ObserverFailure)`, set `state.held_event = Some(seq)`, `break` (no fold, no count).
     `FailOpen` path untouched (byte-for-byte).
   - Held-event re-attempt rule: when `state.held_event == Some(seq)`, the next batch cycle that
     sees `seq` again re-runs the **single-event path only** — deserialize once; if it now
     deserializes, one observer invocation with no retry ladder. Success clears the marker and
     normal delivery resumes from there; failure (deser or observer) re-holds silently (debug log
     only — `on_halt` fires on halt *entry* and on release, not on every re-attempt, to avoid
     alert spam).
   - Panic containment in `process_event_with_retry` (retry.rs): wrap the `on_event` call in
     `futures::FutureExt::catch_unwind` (not `std::panic::catch_unwind` — this wraps a `Future`,
     not a synchronous call; `futures = "0.3"` is already a dependency), mapping a panic payload
     to an observer error (→ existing retry/DLQ machinery; a panicking observer therefore DLQs
     rather than killing the listener — documented FO behaviour change).
   - `HaltCallback`/`HaltInfo`/`HaltReason` in `epoch_pg/src/event_bus/config.rs`
     (`#[non_exhaustive]`, parallel to `DlqCallback`), plus an `on_halt` field added to
     `ReliableDeliveryConfig`. **This is a breaking change and the plan says so plainly** —
     but it is a plain field addition, NOT `#[non_exhaustive]`: marking the struct
     `#[non_exhaustive]` would forbid every struct expression outside this crate *including*
     functional-update syntax (E0639), breaking the ~40 `ReliableDeliveryConfig { … }`
     literals in the test crates (35 in pgeventbus_integration_tests.rs, 5 in
     inline_dispatch_integration_tests.rs) and making this phase's "existing suite green"
     exit unsatisfiable. Repo precedent ships struct-field additions as plain
     source-compat notes (CHANGELOG.md:247-253). So: add the field, add the `on_halt` line
     to the hand-written `Debug` impl (config.rs:273-297), fix BOTH in-crate exhaustive
     literals — `impl Default for ReliableDeliveryConfig` (config.rs:253-271, which must gain
     `on_halt: None` as the field's default source) and the test literal (config.rs:441-455);
     in-crate literal (config.rs:441-455). Commit is `feat(pg)!: …` with the semver note in
     the commit body and a CHANGELOG entry; this is the one config-shape break the plan
     introduces (P4b's later API addition is a new method, which is additive, not this).
3. **Validate:** T1/T2/T3/T6/T8-live green; fail-open regression pins green (existing
   `test_undeserializable_event_advances_past` untouched and passing). **Exit:** R2, R4, R6
   (live), R7, R8 (asserted in T1), R10, R11 (live), R12.
   Commits: `test(pg): …` + `feat(pg)!: …` (config break) — kept as two commits so the test
   scaffolding and the breaking config change are independently reviewable and revertible.

## P3 — Catch-up + drain hold, inline panic wrap

**P0 fix from review round 1 — read before implementing:** a plain `break` of only the row loop
at the catch-up/drain halt sites is a hot infinite loop, not a halt. Both sites are a row loop
nested inside an outer pagination loop whose *only* exits are an empty result set or a short
(sub-`catch_up_batch_size`) batch, with the fetch cursor (`current_sequence`) advanced **only**
inside the row loop:

- Catch-up: outer `loop { … }` at mod.rs:2957, exits at `rows.is_empty()` (mod.rs:2969) or
  `batch_size < catch_up_batch_size` (mod.rs:3060-3062); `current_sequence` is advanced only at
  mod.rs:2991 (success) / 2994-ish (deser-skip, current draft) inside the row `for` loop.
- Drain: outer loop at mod.rs:3389, exits at mod.rs:3410 (empty) / 3501-3503 (short batch);
  `current_sequence` advanced only at mod.rs:3497 inside the row loop.

If a fail-closed halt `break`s only the row loop on a **full** batch (the common case — the
default `catch_up_batch_size` is 100, config.rs:261), the outer loop re-fetches the identical
window starting at the unchanged `current_sequence`, hits the same bad row, and breaks the row
loop again — forever, with no sleep, and `subscribe()` never returns. Fix: introduce a `halted`
flag (or reuse `held_event.is_some()`) set at the bad row; `break` the row loop, **and** check the
flag at the top of the outer pagination loop to exit it too. This does not conflict with R3's
"drain halt still completes registration" rule — the flag exits both loops through their normal
control flow, not via an early `return` out of `subscribe()`, so registration and the final flush
still run afterward exactly as before.

1. **Failing tests:** T4 (catch-up halt: pre-planted checkpoint + corrupt row in history →
   catch-up halts at the bad sequence, no advance, **and a second variant plants more than
   `catch_up_batch_size` (100) rows below the corrupt one**, so the full-batch case is exercised —
   the short-batch case alone would pass trivially and ship the infinite-loop hazard above
   unnoticed; then live drain halt: buffer processing halts, **subscriber still registered**
   — observer present in the live registry and listener-driven afterwards. Full assertion set
   per the spec, not thinned: DLQ row + `on_halt` fired + readiness blocked (R8) + no advance, in
   both the catch-up and drain phases); T8 remainder (panic during catch-up → `subscribe()`
   surfaces a contained error; panic in inline drain → `publish()` returns `Err`, `inline_state`
   cleaned up (mod.rs:2695-2699), no deadlock).
2. **Implement:** catch-up deser branch (mod.rs:2977-3007): `FailClosed` → set the halted marker,
   skip `advance_catchup_prefix` (2994), `break` the row loop, check the marker at the outer loop
   head and `break` it too (final flush safe by construction — pending stays unpublishable).
   Same pattern for the catch-up observer-failure branch. Drain deser (mod.rs:3425-3438) + shared
   tail (3484): `FailClosed` → skip the shared `advance_catchup_prefix` for the failed row,
   `break` both loops via the same marker check, and **do not early-return out of `subscribe()`**
   — registration (`projections.push`, mod.rs:3560) and the post-drain flush still run because
   the loops exit normally. Drain observer-failure: same rule. Inline drain: wrap `on_event` in
   `catch_unwind`, route panics through the existing `Err` branch.
3. **Validate:** T4 (both variants) + T8 remainder green; R3 (incl. registration), R8 (catch-up
   leg), R11 (all paths). Commit: `feat(pg): …`.

## P4 — Gap refusal (`subscriber_state.rs`)

1. **Failing tests:** T5 (isolated table, `claim_hole_uncommitted`, `gap_timeout: 500ms`,
   fencing on: no advance after the backstop window; no `epoch_event_bus_gap_timeouts` row; no
   `on_gap_timeout`; `on_halt(GapUnproven)` fired; **recovery = roll back the held transaction**
   — not commit it. Committing makes the sequence *visible*, so it is delivered as a normal event
   and no `SkippedGap`/`FenceCleared` is ever produced; the fence branch only runs when
   `!visible_seqs.contains(&next)` (subscriber_state.rs:205-241). `FenceCleared` requires the
   writer to abort. Roll back the held transaction (or roll back a second writer after
   re-pinning) → the sequence provably never existed → `FenceCleared` fires → advance). Fail-open
   gap regression pin (backstop advances + row + callback as today, unchanged).
2. **Implement:** `advance_contiguous_checkpoint` gains a `failure_mode: FailureMode` parameter
   (pub(crate)). Real cost, stated plainly: there is exactly **one** production call site
   (mod.rs:342) but **~21** call sites inside the function's own `#[cfg(test)]` module
   (subscriber_state.rs:335 through :743) — Rust has no default arguments, so all ~21 need a
   `FailureMode::FailOpen` argument added in this same commit. If that mechanical sweep is
   disruptive in review, the fallback is a thin `FailOpen`-defaulting wrapper used by the
   existing unit tests and the new parameter only exercised directly by the new fail-closed unit
   tests — implementer's call, but plan for the sweep either way, not as an afterthought. Backstop
   branch (subscriber_state.rs:246-262): `FailClosed` → `break` — no `SkippedGap` pushed,
   `gap_first_seen` kept, fence not re-captured. Fence branch (:222-241) untouched.
3. **Validate:** T5 + pins green; R5, R7 (gap-refusal leg). Commit: `feat(pg): …`.

## P4b — Wedged-subscriber private fetch + release operation

Scoped to `Checkpointed` subscribers only; the `ReplayAlways` analogue is P5's job (see P5's
exit criteria — a wedged `ReplayAlways` subscriber still needs floor exclusion once P5 gives it a
contiguous hold, even though it needs no release API).

**Known intra-pipeline gap (P2 → P5):** a fail-closed `ReplayAlways` subscriber that halts on the
LIVE path acquires a floor-pinning hold as early as P2 — the live batch maintains
`state.contiguous_checkpoint` identically for both modes (mod.rs:247-259, 480-499; only the
flush sink differs), and the floor at mod.rs:1509-1513 has no mode filter. P4b's exclusion is
Checkpointed-scoped, so between P2 and P5 a wedged ReplayAlways subscriber would starve peers.
This window never ships: P5 lands in the same pipeline before any release, and the gap is
closed there (floor exclusion for held ReplayAlways HWMs). Documented here so it is a known,
bounded window, not a surprise.

1. **Failing tests:** T9 — run under the isolation standing rule (isolated table or `#[serial]`
   with a pre-planted checkpoint), in **two** variants so the wedged predicate below is proven
   for both halt kinds: (a) a refused-gap wedge via `claim_hole_uncommitted`; (b) a deser wedge
   (corrupt row, relies on P2's `held_event` marker being set on the deser path — see FIX-6/step 2
   below). Both: healthy peer on the same bus; commit events well beyond `catch_up_batch_size`
   past the wedge point X → peer advances past X + `catch_up_batch_size` while the wedge holds
   (R13); then invoke `release_halt(sid, > X)` → wedged subscriber resumes without a restart,
   with **no duplicate delivery** of any `processed_ahead` entries applied above a refused gap
   while wedged (R14 + R6). Separate unit test: an all-wedged bus skips the shared fetch entirely
   for that cycle and is driven by private fetches, one pass over every wedged subscriber, then
   the cycle ends and the timer tick re-enters it — this is what "the shared-row break conditions
   (mod.rs:1536/1660) do not apply" concretely means; it is not a claim that the loop hangs.
2. **Implement:**
   - Wedged predicate on `SubscriberState`: `FailClosed && (held_event.is_some() || a refused gap
     in gap_first_seen)`. Because P2's deser branch (not just the observer branch) now also sets
     `held_event` on halt, a deser wedge is visible to this predicate exactly like an
     observer-failure wedge — this is why P2's step 2 was written that way. Fail-open transient
     gaps do NOT count (R10: today's floor behaviour preserved for FO subscribers).
   - Shared floor: `min` over non-wedged states only (mod.rs:1509-1513). Extract the floor
     computation and the "skip shared fetch this cycle" decision into a `pub(crate)` pure
     function taking the subscriber-state snapshot and returning `Option<u64>` (the floor, or
     `None` for "everyone is wedged, skip the shared fetch") — this is what makes the all-wedged
     case unit-testable without spinning up a bus; the code today is inline in the spawned
     listener body (mod.rs:1500-1665) with `sqlx` calls interleaved, so this extraction is a real
     (small) refactor, not free.
   - Private fetch per wedged subscriber: cursor = persisted checkpoint (re-read each cycle),
     cap = `catch_up_batch_size`, builds its OWN `visible_seqs` from its own query's rows —
     **and**, under the same gate the shared path already uses (`snapshot_fencing && any
     gap_first_seen non-empty`, mod.rs:1569-1577), calls `query_txid_snapshot` for its own txid
     snapshot every cycle, or a gap-wedged subscriber's `FenceCleared` recovery can never fire
     from the private path. State fully preserved across re-seeds — the re-seed only moves the
     *fetch cursor*, never discards bookkeeping: contiguous position, `processed_ahead` (already-
     applied events above a refused gap — discarding would re-deliver them, breaking R6),
     `gap_first_seen` including the first-captured `fence_xmax` (re-capturing it against a moving
     `xmax` every cycle would mean it never clears on a bus with continuous write traffic), and
     `held_event`. Forward reconciliation: if the persisted cursor has moved past the held
     position (an operator release), fold `processed_ahead` entries ≤ the new cursor, adopt the
     cursor, retain any remaining `processed_ahead` entries above it.
   - Release API: `pub async fn release_halt(&self, subscriber_id: &str, past_sequence: u64) ->
     Result<(), PgEventBusError>` — named to avoid colliding with the existing
     `release_subscriber_lock` (mod.rs:2535, an unrelated coordinated-mode advisory-lock release).
     Forward-only vs the persisted row; a backward `past_sequence` is rejected with an error
     explaining the R14 semantics (a release accepts a skip past a sequence the subscriber never
     finished — it is not a rewind). On success: writes the checkpoint row, WARN + `on_halt`
     with `HaltReason::Released`. Purely additive API — no existing signature changes.
     `update_checkpoint` untouched.
3. **Validate:** T9 (both wedge variants) + the all-wedged unit test green; R13/R14/R6. Commit:
   `feat(pg): …` (additive — the breaking config change already landed in P2).

## P5 — ReplayAlways contiguous HWM

1. **Failing tests:** T7 (listener restart re-seeds surviving HWM, hold persists, re-attempt);
   T7b (fresh `subscribe()` → HWM reset 0 (mod.rs:3251) → full replay re-holds at the same bad
   row, nothing applied above it); **T10 (new): a wedged fail-closed `ReplayAlways` subscriber
   does not starve a healthy peer** — commit events well beyond `catch_up_batch_size` past the
   wedge, assert the peer keeps advancing. This closes a gap the plan otherwise leaves open:
   `subscriber_states` is seeded for `ReplayAlways` subscribers too (`contiguous_checkpoint` =
   the HWM value, mod.rs:1436-1450) and the floor computation (mod.rs:1509-1513) has no mode
   filter, so once this phase gives `ReplayAlways` a contiguous hold, a wedged one pins the
   bus-wide floor exactly like a `Checkpointed` one would without P4b's exclusion. P4b's
   exclusion is `Checkpointed`-scoped and lands before this phase, so without T10 the plan would
   ship the same starvation bug it designed P4b against, just for the other subscription mode.
2. **Implement:** `advance_catchup_prefix` (mod.rs:2828-2833) — currently sets
   `hwm = event_global_seq` before the `!= *contiguous + 1` guard; route the ReplayAlways sink
   through the same contiguous-prefix hold logic as checkpoints (hwm advances only across the
   unbroken prefix). Live path untouched (already contiguous via `new_contiguous`,
   mod.rs:494-499). **Also extend the wedged predicate/floor exclusion from P4b to a held
   `ReplayAlways` HWM** — same shared-floor exemption, no release API (a wedged `ReplayAlways`
   subscriber's remedy stays the fresh-`subscribe()` full replay per R9b; it has no persisted
   checkpoint row for `release_halt` to act on). Closes CLOUD-227 (verify its ticket wording at
   commit time).
3. **Validate:** T7/T7b/T10 green; R9(a)/(b), R13 extended to `ReplayAlways`. Commit:
   `fix(pg): …` (or `feat(pg)` if CLOUD-227 reads as a feature).

## P6 — Regression pins, docs, hardening

1. **Failing tests (pins):** FO deser-skip advances past (exists — keep green); FO observer
   failure → DLQ + continue + advance; FO `TimeoutBackstop` → advance + row + callback;
   `FenceCleared` advances under both modes; `update_checkpoint` behaviour unchanged.
2. **Implement:** rustdoc pass — `FailureMode` (R12: frozen-read-model cross-group consequence,
   self-healing recovery, operator release), `HaltCallback`/`HaltInfo`/`HaltReason`,
   `release_halt`, `held_event` internals (pub(crate) docs), the `ReliableDeliveryConfig`
   breaking field-addition semver note. Changelog entry per repo convention.
3. **Validate:** full `cargo test` (all crates), `cargo clippy -- -D warnings`, `cargo fmt
   --check`, doc build without warnings. Commit: `docs(pg): …` + `test(pg): …`.

## Sequencing, risk, and rollback

- Order is dependency-ordered: P1 → P2 → P3 → P4 → P4b → P5 → P6. P4 and P4b are sequential
  (P4b's wedged predicate needs P4's gap-refusal bookkeeping); P5 depends on P4b's extraction
  (the floor-exclusion function) but not on P4's gap logic — could parallelize P4/P4b vs. an
  early start on P5's HWM mechanics with worktrees, but single-writer per the loop policy is fine
  at this size.
- Highest-risk phase: P4b (touches the shared fetch loop, requires extracting the floor/skip
  decision into a testable pure function — a real refactor, not a pure addition). Mitigation:
  T9's peer-liveness assertion, the all-wedged unit test against the extracted function, and
  keeping the shared-fetch code path byte-identical when no subscriber is wedged.
- **ReplayAlways floor gap (P2 → P5 window):** between P2 and P5, a wedged fail-closed
  ReplayAlways subscriber pins the shared floor for peers (live-path holds predate P5's
  exclusion). Bounded and intra-pipeline — P5 closes it before anything ships; noted in P4b's
  scope statement.
- Behaviour changes owned (documented, reviewed):
  - `ReliableDeliveryConfig` gains the `on_halt` field — a breaking change handled as a plain
    field addition (no `#[non_exhaustive]`; see P2), `feat(pg)!` + CHANGELOG semver note.
  - FO panic → DLQ instead of listener death (P2).
  - `release_halt` is a new, purely additive public API (P4b).
  - No other FO behaviour changes — the R10 pins guard this.
- Rollback: each phase is an independent commit; any phase can be reverted without breaking
  earlier ones, with two exceptions: P3 depends on P2's `held_event`/halt machinery existing, and
  P4b depends on P2 (the `held_event` marker on both halt kinds) and P4 (gap bookkeeping) — revert
  P4b before P4 or P2 if backing out.
