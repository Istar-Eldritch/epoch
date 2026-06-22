# Spec 0023: `epoch` — versioned snapshot store with configurable capture & retention

**Issue:** Linear [CLOUD-184](https://linear.app/catallactical/issue/CLOUD-184)
**Title:** epoch: add versioned snapshot store with configurable capture & retention
**Status:** Proposed (ready for review)
**Created:** 2026-06-21
**Ticket state:** Triage, Priority High, Unassigned
**Discovery:** `specs/0023-cloud184-discovery.md`
**Crates:** `epoch_core` (traits + config + `state_at`), `epoch_pg` (impl + migration m013), `epoch_mem` (impl), `aggregate` concept (capture/prune integration)
**Commit scope:** `feat(core)` / `feat(pg)` / `feat(mem)` / `feat(aggregate)`
**Workspace version:** `0.1.0` (pre-1.0)
**Depends on:** Spec 0022 (CLOUD-183 `read_events_range`) — **for the `state_at` helper only** (§4.6, §11 Phase 6). See §3.4 / §9 for the dependency status and interim handling.
**Numbering alignment:** Migrations m011 (spec 0019, CLOUD-180), m012 (CLOUD-173) are already on `main`; this spec claims **m013** (verified §3.3).

---

## 1. Problem Statement

`StateStoreBackend<S>` (`epoch_core/src/state_store.rs:9`) is a **single-snapshot**
live-state store: `persist_state` upserts the latest state, overwriting whatever was
there. There is no historical, version-keyed copy of an aggregate's state.

Two consequences follow:

1. **Cold rehydration is O(N) in stream length, forever.** `Aggregate::handle()`
   (`epoch_core/src/aggregate.rs:~360`) re-hydrates by reading the live state and then
   replaying every subsequent event. For a brand-new cold load with no persisted state,
   recovery, or any consumer that needs to rebuild from scratch, the only path is a full
   replay from version 0. Every mature event-sourcing framework (Axon, EventStoreDB,
   Marten) ships snapshotting to bound this; epoch has none.

2. **Time-travel (`state_at(version)`) has no fast path.** Reconstructing the state of an
   aggregate *as of* a specific `stream_version` currently means replaying the whole
   stream up to `V`. The natural optimization — load the nearest snapshot `≤ V`, then
   replay only `(snapshot_version, V]` — is impossible without a versioned snapshot store.

This ticket adds a first-class, **opt-in** versioned snapshot capability: a
`SnapshotStore<S>` trait for version-keyed historical state, a `SnapshotConfig` exposing
capture-trigger and retention policy, automatic capture/prune wired into
`Aggregate::handle()` *only when configured*, and a `state_at(version)` composition helper
that pairs the snapshot fast-path with CLOUD-183's bounded reads.

---

## 2. Goals / Non-Goals

### Goals

- **G-1** Add `SnapshotStore<S>` to `epoch_core` as a **new, standalone** trait, distinct
  from `StateStoreBackend` (which is left entirely unchanged).
- **G-2** Add `SnapshotConfig` / `SnapshotTrigger` / `SnapshotRetention` / `Snapshot<S>`
  value types to `epoch_core`. First iteration ships `Manual` + `Automatic { interval }`
  and `Unlimited` + `KeepLast(n)`; the enums are `#[non_exhaustive]`-friendly (extensible
  without a breaking change).
- **G-3** Implement `PgSnapshotStore<S>` (`epoch_pg`) backed by a new `epoch_snapshots`
  table created by **migration m013**, plus `InMemorySnapshotStore<S>` (`epoch_mem`).
- **G-4** Integrate capture + prune into `Aggregate::handle()` behind an **opt-in,
  defaulted no-op hook**, so aggregates with no snapshot config are **byte-for-byte
  identical** to today (no extra I/O, no behaviour change). Add a manual
  `save_snapshot(id)` path.
- **G-5** Provide `state_at(version)` that returns state **equivalent to a full replay
  from zero**, composing `load_snapshot(≤V)` with CLOUD-183's `read_events_range`.
- **G-6** Additive only: no breaking change to any existing public trait. The existing
  test suite passes **unmodified**; `cargo clippy --all-targets -- -D warnings` and
  `cargo fmt --check` are clean.

### Non-Goals

- **NG-1** No change to `StateStoreBackend` or its existing impls.
- **NG-2** No `TimeWindow(Duration)` / `PerVersionInterval` retention/trigger variants in
  this iteration (the enums are left extensible for them; §4.2).
- **NG-3** No automatic snapshot-backed fast path *inside* `handle()`'s own re-hydration
  read in this iteration. `handle()` re-hydrates from live state + tail events exactly as
  today; the snapshot fast-path is exposed via `state_at` and is available to consumers,
  but rewiring `handle()`'s hot read to prefer a snapshot is deliberately deferred to keep
  the byte-for-byte guarantee (G-4) auditable. (Called out as OQ-1.)
- **NG-4** No custom snapshot table name (`with_table`) for `PgSnapshotStore`; the table is
  the fixed `epoch_snapshots`. (Future; §8.)
- **NG-5** No `created_at`-driven retention logic in v1 (the column is stored for forward
  compatibility but no policy consumes it yet).
- **NG-6** Sentinel's `evaluate_at` / `scope_at` — the downstream consumer, tracked
  separately.
- **NG-7** A shared `epoch_core::testing` snapshot-parity helper — kept out of scope to
  match the per-backend test instruction and the precedent set by spec 0022 §8 (§8).

---

## 3. Current-State Anchors (verified 2026-06-21)

### 3.1 `epoch_core`

- `StateStoreBackend<S>` trait (single-snapshot, unchanged here) —
  `epoch_core/src/state_store.rs:9`.
- `EventApplicator<ED>` with `re_hydrate` (replays a `Pin<Box<dyn EventStream>>` into
  state) — `epoch_core/src/event_applicator.rs:~190`. `state_at` reuses this verbatim.
- `Aggregate<ED>::handle()` orchestration; the live-state `persist_state` /
  `delete_state` block at the tail (the only place state is written) —
  `epoch_core/src/aggregate.rs:~450`. `new_state_version` is the last event's
  `stream_version`; `events.len()` is the count applied this command.
- `AggregateState::get_version` / `set_version` — `epoch_core/src/aggregate.rs:60`.
- `lib.rs` module list + `prelude` re-exports — `epoch_core/src/lib.rs:23`, `:40`.
  New `snapshot` module must be added to both.
- `#![deny(missing_docs)]` is in force (`epoch_core/src/lib.rs:21`) — every new public
  item needs rustdoc.
- Reusable contract-test helpers live in `epoch_core/src/testing.rs` behind the `testing`
  feature (precedent: `verify_store_events_atomicity`). Not extended here (NG-7).

### 3.2 `EventStoreBackend` read primitives (the `state_at` substrate)

- Trait `EventStoreBackend` — `epoch_core/src/event_store.rs:25`. Current read methods are
  `read_events` (`:33`) and `read_events_since` (`:39`). **`read_events_range` does not yet
  exist** (see §3.4).

### 3.3 `epoch_pg` migrations

- Latest registered migration is **m012** (`add_schema_version_to_events`, version 12) —
  `epoch_pg/src/migrations/mod.rs` (`MIGRATIONS` array ends at `&AddSchemaVersionToEvents`).
  **This work is m013** (`create_snapshots_table`, version 13).
- `MIGRATION_COUNT` auto-tracks `MIGRATIONS.len()` (`migrations/mod.rs`) — adding m013
  updates it for all consumers; no hardcoded count literal to bump.
- Table-creation migration template: m009 `CreateGapTimeoutLog` uses plain
  `CREATE TABLE IF NOT EXISTS` + `CREATE INDEX IF NOT EXISTS`
  (`epoch_pg/src/migrations/m009_create_gap_timeout_log.rs`). m013 follows **this**
  pattern, **not** the m011/m012 `to_regclass('epoch_events')` rename-guard pattern — the
  rename guard exists only because m011/m012 *alter* `epoch_events`; m013 creates a brand
  new table and needs no such guard.
- Migration integration test asserts each version/name via `applied_after[i]` and
  `applied.len() == MIGRATION_COUNT` — `epoch_pg/tests/migrations_integration_tests.rs`
  (the chain currently ends at `applied_after[11]` = version 12,
  `add_schema_version_to_events`, `:325`). m013 adds an `applied_after[12]` assertion
  (version 13, `create_snapshots_table`).
- `PgStateStore<T>` / `PgState` upsert pattern (the bespoke domain-table path snapshots are
  *decoupled* from) — `epoch_pg/src/state_store.rs`.

### 3.4 CLOUD-183 (`read_events_range`) status — **not yet landed**

`read_events_range` is specified in `specs/0022-cloud183-read-events-range.md` but **is not
present in the codebase** (verified: no match for `read_events_range` in any `*.rs`). The
`state_at` helper (§4.6) is the only part of this ticket that depends on it. Consequences:

- The snapshot store, config, both backends, and the aggregate capture/prune integration
  (Phases 1–5) are **fully independent of CLOUD-183** and can land first.
- The `state_at` helper (Phase 6) **requires** `read_events_range(stream_id, Some(from),
  Some(to))`. Two acceptable paths, decided at implementation time:
  - **Preferred:** land CLOUD-183 first (or co-deliver it), then `state_at` calls
    `read_events_range` directly.
  - **Interim (only if CLOUD-183 has not merged):** `state_at` calls
    `read_events_since(stream_id, from)` and truncates the stream at `to` in user-space
    (correct, just not I/O-optimal), with a `// TODO(CLOUD-183)` to swap to
    `read_events_range`. This is explicitly the over-read CLOUD-183 eliminates, so it is a
    temporary bridge, not the target state.

This dependency is restated in §9 and isolated to the final phase in §11.

---

## 4. Design

### 4.1 New module: `epoch_core/src/snapshot.rs`

All new core types live in one new file (per the natural-growth convention; it can split
later). `lib.rs` gains `pub mod snapshot;` and `prelude` re-exports `snapshot::*`.

### 4.2 Config value types (`epoch_core`)

```rust
/// Configures snapshot capture and retention for an aggregate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotConfig {
    /// When automatic snapshots are taken.
    pub trigger: SnapshotTrigger,
    /// How many snapshots to retain per stream.
    pub retention: SnapshotRetention,
}

/// When snapshots are captured.
///
/// Marked `#[non_exhaustive]` so future variants (e.g. `TimeWindow(Duration)`,
/// `PerVersionInterval(u64)`) can be added without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SnapshotTrigger {
    /// No automatic snapshots; the caller invokes `save_snapshot()` explicitly.
    Manual,
    /// Capture a snapshot automatically when an `interval`-version boundary is crossed.
    ///
    /// `interval == 0` is treated as "never" (no automatic capture) and never divides.
    Automatic { interval: u64 },
}

/// How many snapshots to keep per stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SnapshotRetention {
    /// Keep every snapshot ever taken.
    Unlimited,
    /// Keep only the most recent `n` snapshots per stream (by `version` descending).
    KeepLast(u32),
}

/// A version-keyed historical copy of aggregate state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot<S> {
    /// The `stream_version` this snapshot represents.
    pub version: u64,
    /// The state as of `version`.
    pub state: S,
}
```

`#[non_exhaustive]` on the enums delivers the discovery's "deliberately extensible"
requirement: downstream `match`es must include a wildcard arm, so adding a variant later is
non-breaking. `Default` is **not** derived for `SnapshotConfig` (there is no obviously
correct default trigger; the consumer chooses).

### 4.3 `SnapshotStore<S>` trait (`epoch_core`)

```rust
#[async_trait]
pub trait SnapshotStore<S>: Send + Sync {
    /// Error type for snapshot operations.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Loads the most recent snapshot at or before `target_version`.
    ///
    /// Returns `Ok(None)` if no snapshot exists for `stream_id` at or before the target.
    async fn load_snapshot(
        &self,
        stream_id: Uuid,
        target_version: u64,
    ) -> Result<Option<Snapshot<S>>, Self::Error>;

    /// Saves a snapshot at `version`. **Idempotent** per `(stream_id, version)`:
    /// re-saving the same version overwrites the stored state and must not error.
    async fn save_snapshot(
        &self,
        stream_id: Uuid,
        version: u64,
        state: &S,
    ) -> Result<(), Self::Error>;

    /// Prunes snapshots beyond what `policy` permits, keeping the most recent allowed set.
    /// A no-op for [`SnapshotRetention::Unlimited`].
    async fn apply_retention(
        &self,
        stream_id: Uuid,
        policy: &SnapshotRetention,
    ) -> Result<(), Self::Error>;
}
```

The trait imposes **no serde bound** on `S`; serialization requirements belong to concrete
impls (`epoch_pg` adds `Serialize + DeserializeOwned`; `epoch_mem` needs only `Clone`).

### 4.4 `epoch_mem::InMemorySnapshotStore<S>`

Backed by `Arc<Mutex<HashMap<Uuid, Vec<Snapshot<S>>>>>`, mirroring the existing
`InMemoryStateStore` shape (`epoch_mem/src/event_store.rs:731`). `S: Clone + Send + Sync`.

- `load_snapshot(id, target)`: among the stream's snapshots with `version <= target`, return
  the one with the **highest** version (cloned); `None` if none.
- `save_snapshot(id, version, state)`: replace any existing snapshot at `version`, else
  insert; keep the per-stream `Vec` sorted by version (idempotent).
- `apply_retention(id, KeepLast(n))`: retain the `n` highest versions, drop the rest.
  `Unlimited`: no-op.

`InMemorySnapshotStore` and `InMemorySnapshotStoreError` are re-exported from
`epoch_mem/src/lib.rs`.

### 4.5 `epoch_pg::PgSnapshotStore<S>` + migration m013

**Migration m013 — `epoch_pg/src/migrations/m013_create_snapshots_table.rs`** (`version() ==
13`, `name() == "create_snapshots_table"`), registered as the new last entry of
`MIGRATIONS`. Plain `CREATE TABLE IF NOT EXISTS` (m009 pattern — no rename guard):

```sql
CREATE TABLE IF NOT EXISTS epoch_snapshots (
    stream_id  UUID        NOT NULL,
    version    BIGINT      NOT NULL,
    data       JSONB       NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (stream_id, version)
);

-- Supports `WHERE stream_id = $1 AND version <= $2 ORDER BY version DESC LIMIT 1`.
CREATE INDEX IF NOT EXISTS idx_epoch_snapshots_stream_version
    ON epoch_snapshots (stream_id, version DESC);
```

The composite `PRIMARY KEY (stream_id, version)` gives `save_snapshot` its idempotency
target and already orders by `(stream_id, version)`; the explicit `version DESC` index makes
the nearest-`≤` lookup a single index seek. Forward-only (no `down()`).

**`PgSnapshotStore<S>`** — `epoch_pg/src/snapshot_store.rs` (new), `S: Serialize +
DeserializeOwned + Send + Sync`. Holds a `PgPool`. `type Error = sqlx::Error`.

- `load_snapshot`:
  `SELECT version, data FROM epoch_snapshots WHERE stream_id = $1 AND version <= $2
   ORDER BY version DESC LIMIT 1`; bind `target_version as i64`; deserialize `data`
  (JSONB) into `S`; map row → `Snapshot { version, state }`.
- `save_snapshot`:
  `INSERT INTO epoch_snapshots (stream_id, version, data) VALUES ($1, $2, $3)
   ON CONFLICT (stream_id, version) DO UPDATE SET data = EXCLUDED.data, created_at = NOW()`
  (serialize `state` to `serde_json::Value`). Idempotent.
- `apply_retention(id, KeepLast(n))`:
  `DELETE FROM epoch_snapshots WHERE stream_id = $1 AND version NOT IN
   (SELECT version FROM epoch_snapshots WHERE stream_id = $1 ORDER BY version DESC LIMIT $2)`;
  `Unlimited` → `Ok(())` without a query. `n == 0` is permitted (deletes all but zero, i.e.
  empties the stream's snapshots) — documented; callers wanting "keep everything" use
  `Unlimited`.

`PgSnapshotStore` is re-exported from `epoch_pg/src/lib.rs`. The table name is fixed
`epoch_snapshots`; unlike the events table there is no per-store custom-table option in v1
(NG-4), so no `ensure_*` startup helper is required.

### 4.6 `state_at(version)` helper (`epoch_core::snapshot`)

A standalone generic async **free function** (not a method on `EventApplicator` — that trait
is implemented by projections too, which must not gain snapshot dependencies):

```rust
/// Reconstructs the state of `stream_id` as of `version`, equivalent to a full
/// replay from zero but using the nearest snapshot `<= version` as a fast start.
pub async fn state_at<ED, A, ES, SS>(
    applicator: &A,
    event_store: &ES,
    snapshot_store: &SS,
    stream_id: Uuid,
    version: u64,
) -> Result<Option<A::State>, StateAtError<SS::Error, ES::Error, A::ApplyError>>
where
    ED: EventData + Send + Sync + 'static,
    A: EventApplicator<ED> + Sync,
    ES: EventStoreBackend<EventType = ED> + Sync,
    SS: SnapshotStore<A::State> + Sync,
{
    // 1. nearest snapshot <= version
    let snap = snapshot_store
        .load_snapshot(stream_id, version)
        .await
        .map_err(StateAtError::Snapshot)?;
    let (start_state, from) = match snap {
        Some(s) => (Some(s.state), s.version + 1),
        None => (None, 1),
    };
    // 2. bounded replay (from, version]  — CLOUD-183 read_events_range (§3.4)
    let stream = event_store
        .read_events_range(stream_id, Some(from), Some(version))
        .await
        .map_err(StateAtError::EventStore)?;
    // 3. fold events onto the start state
    applicator
        .re_hydrate(start_state, stream)
        .await
        .map_err(StateAtError::Hydrate)
}
```

`StateAtError<SnapErr, EsErr, ApplyErr>` is a new `thiserror` enum with `Snapshot`,
`EventStore`, and `Hydrate` variants (the last wrapping `ReHydrateError`). The
**equivalence contract** (G-5) is the testable property: for every `v`, `state_at(.., v)`
equals replaying `read_events_range(.., None, Some(v))` from `None` with no snapshot in play
(§6).

> **CLOUD-183 dependency:** `read_events_range` must exist (§3.4). If it has not landed when
> this phase is implemented, use the documented interim (`read_events_since` + user-space
> truncation at `to`) and leave a `TODO(CLOUD-183)`.

### 4.7 Aggregate integration (the byte-for-byte-sensitive change)

**Recommended mechanism — a defaulted no-op hook on `Aggregate` + a `SnapshottingAggregate`
extension trait.** This keeps the base-trait footprint to a single defaulted method that
*does nothing* by default, so non-snapshotting aggregates are provably unchanged, while the
reusable capture/prune logic lives in the extension trait.

**1. Defaulted hook on `Aggregate<ED>`** (`epoch_core/src/aggregate.rs`):

```rust
/// Invoked by `handle()` immediately after the canonical event + live-state writes
/// succeed (state-present branch only). Default: **no-op**.
///
/// Snapshotting aggregates override this to delegate to
/// [`SnapshottingAggregate::capture_snapshot_if_due`]. Returns `()`: a snapshot
/// failure must never fail an already-committed command (events are durable; a
/// snapshot is a rebuildable cache), so implementors log and swallow errors.
async fn after_persist(
    &self,
    _stream_id: Uuid,
    _new_version: u64,
    _events_applied: usize,
    _state: &<Self as EventApplicator<ED>>::State,
) {
    // no-op
}
```

`handle()` calls it once, in the **state-present** branch, after `persist_state` succeeds:

```rust
state_store.persist_state(*state.get_id(), state.clone()).await.map_err(...)?;
self.after_persist(state_id, new_state_version, events.len(), &state).await; // NEW
Ok(Some(state))
```

The `delete_state` branch is left untouched (a deleted aggregate takes no snapshot).
Because the default body is empty and is the only addition, aggregates that do not override
`after_persist` perform **zero** extra work — no I/O, no allocation, no observable behaviour
change (the byte-for-byte guarantee, G-4).

**2. `SnapshottingAggregate<ED>` extension trait** (`epoch_core/src/snapshot.rs`):

```rust
#[async_trait]
pub trait SnapshottingAggregate<ED>: Aggregate<ED>
where
    ED: EventData + Send + Sync + 'static,
    <Self as EventApplicator<ED>>::State: AggregateState,
{
    /// The snapshot store backing this aggregate.
    type SnapshotStore: SnapshotStore<<Self as EventApplicator<ED>>::State> + Send + Sync;

    /// Returns the snapshot store.
    fn snapshot_store(&self) -> Self::SnapshotStore;
    /// Returns the snapshot capture/retention config.
    fn snapshot_config(&self) -> &SnapshotConfig;

    /// Captures + prunes per config. Call from `Aggregate::after_persist`.
    async fn capture_snapshot_if_due(
        &self,
        stream_id: Uuid,
        new_version: u64,
        events_applied: usize,
        state: &<Self as EventApplicator<ED>>::State,
    ) {
        let config = self.snapshot_config();
        let interval = match config.trigger {
            SnapshotTrigger::Automatic { interval } if interval > 0 => interval,
            _ => return, // Manual or interval == 0: no automatic capture
        };
        // Capture iff an `interval` boundary lies in (prev_version, new_version].
        let prev = new_version.saturating_sub(events_applied as u64);
        if new_version / interval == prev / interval {
            return; // no boundary crossed this command
        }
        let store = self.snapshot_store();
        if let Err(e) = store.save_snapshot(stream_id, new_version, state).await {
            log::warn!("snapshot capture failed for {stream_id}@{new_version}: {e}");
            return;
        }
        if let Err(e) = store.apply_retention(stream_id, &config.retention).await {
            log::warn!("snapshot retention failed for {stream_id}: {e}");
        }
    }

    /// Manual path: load current live state and persist it as a versioned snapshot.
    async fn save_snapshot(
        &self,
        id: Uuid,
    ) -> Result<(), SaveSnapshotError<
        <Self::StateStore as StateStoreBackend<Self::State>>::Error,
        <Self::SnapshotStore as SnapshotStore<Self::State>>::Error,
    >> {
        let state = self
            .get_state_store()
            .get_state(id)
            .await
            .map_err(SaveSnapshotError::State)?
            .ok_or(SaveSnapshotError::NoState)?;
        let version = state.get_version();
        self.snapshot_store()
            .save_snapshot(id, version, &state)
            .await
            .map_err(SaveSnapshotError::Snapshot)
    }
}
```

A concrete snapshotting aggregate bridges the two with a one-line `after_persist`:

```rust
async fn after_persist(&self, stream_id: Uuid, new_version: u64, events_applied: usize, state: &MyState) {
    self.capture_snapshot_if_due(stream_id, new_version, events_applied, state).await;
}
```

**Capture rule rationale.** `prev = new_version - events_applied` is the version *before*
this command. Capturing when `new_version / interval != prev / interval` fires exactly when
a multiple of `interval` falls in `(prev, new_version]` — fully local (no extra read),
deterministic, and idempotent-friendly (re-running the same command re-saves the same
`(stream_id, new_version)`, which `save_snapshot` absorbs). At most one snapshot per command,
always at the latest version.

**Considered alternative (rejected):** a single `Aggregate` accessor returning
`Option<(&dyn SnapshotStore<..>, &SnapshotConfig)>`. Rejected because `SnapshotStore<S>` has
an associated `Error` type and is generic over `S`, making a trait object awkward and
forcing dynamic dispatch; the extension-trait keeps everything statically typed and the
base-trait change to a single empty-bodied method. (Either is acceptable per the discovery;
this spec recommends the extension trait. Deriving the `after_persist` bridge via a macro is
a possible future ergonomic improvement, out of scope here.)

`SaveSnapshotError<StateErr, SnapErr>` is a new `thiserror` enum (`State`, `NoState`,
`Snapshot`) in `snapshot.rs`.

---

## 5. Requirements

| ID | Requirement |
|----|-------------|
| R-1 | `epoch_core::snapshot` defines `SnapshotConfig`, `SnapshotTrigger` (`Manual`, `Automatic { interval }`, `#[non_exhaustive]`), `SnapshotRetention` (`Unlimited`, `KeepLast(n)`, `#[non_exhaustive]`), and `Snapshot<S>`, all with rustdoc; re-exported in `prelude`. |
| R-2 | `epoch_core::snapshot::SnapshotStore<S>` trait exists with `load_snapshot`, `save_snapshot` (idempotent per `(stream_id, version)`), `apply_retention`; imposes no serde bound on `S`. |
| R-3 | `InMemorySnapshotStore<S>` (`epoch_mem`) implements `SnapshotStore<S>`: `load_snapshot` returns the nearest `≤ target`; `save_snapshot` is idempotent; `KeepLast(n)` prunes to the `n` highest versions; `Unlimited` is a no-op. Re-exported from `epoch_mem`. |
| R-4 | Migration **m013** `create_snapshots_table` (version 13) creates `epoch_snapshots (stream_id, version, data jsonb, created_at)` with `PRIMARY KEY (stream_id, version)` and an `(stream_id, version DESC)` index; registered last in `MIGRATIONS`; forward-only. |
| R-5 | `PgSnapshotStore<S>` (`epoch_pg`, `S: Serialize + DeserializeOwned`) implements `SnapshotStore<S>` with the §4.5 SQL; `load_snapshot` is a single sargable index seek; `save_snapshot` uses `ON CONFLICT DO UPDATE`; `KeepLast(n)` deletes all but the `n` highest versions. Re-exported from `epoch_pg`. |
| R-6 | `Aggregate<ED>` gains a defaulted **no-op** `after_persist` hook; `handle()` calls it once in the state-present branch after `persist_state`. The `delete_state` branch is unchanged. |
| R-7 | **Byte-for-byte backward compat:** an aggregate that does not override `after_persist` (no snapshot config) performs zero extra I/O and behaves identically to today; the existing test suite passes **unmodified**. |
| R-8 | `SnapshottingAggregate<ED>` extension trait provides `capture_snapshot_if_due` (capture iff an `interval` boundary is crossed; prune per retention; failures logged, never fatal) and `save_snapshot(id)` (manual path). `interval == 0` ⇒ never captures (no division). |
| R-9 | `state_at(applicator, event_store, snapshot_store, stream_id, version)` returns state **equivalent to a full replay from zero**, loading the nearest snapshot `≤ version` then bounded-replaying `(snapshot_version, version]` via `read_events_range` (CLOUD-183; §3.4 interim if unlanded). |
| R-10 | New tests: capture interval honoured; `KeepLast(n)` prunes correctly; `load_snapshot` returns nearest `≤`; `state_at(v)` matches replay-from-zero for all `v` (deterministic property-style); no-config aggregate identical to baseline. |
| R-11 | Additive only — no breaking change to any existing public trait. `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` clean; full `cargo test` green. |

---

## 6. Test Plan

### 6.1 `epoch_core` (unit, in `snapshot.rs` `#[cfg(test)]`)

- Capture-boundary arithmetic helper (if extracted): `prev/interval != new/interval` fires
  exactly when a multiple of `interval` is crossed (table-driven cases incl. `events_applied
  > interval`, exact-boundary`new_version == k*interval`, and`interval == 0`).
- `SnapshotRetention` / `SnapshotTrigger` `#[non_exhaustive]` round-trips and `Debug`.

### 6.2 `epoch_mem` (unit, in `event_store.rs` or a new `snapshot.rs` `#[cfg(test)]`)

- `load_snapshot` nearest-`≤`: store snapshots at v2, v5, v9; assert `load(4)→v2`,
  `load(5)→v5`, `load(100)→v9`, `load(1)→None`.
- `save_snapshot` idempotency: save v5 twice with different state; second wins; only one
  entry at v5.
- `KeepLast(2)` after snapshots at v2,v5,v9 leaves {v5,v9}; `Unlimited` leaves all three;
  `KeepLast(0)` empties.
- **`state_at` equivalence (property-style, no DB):** build a stream of N events in an
  `InMemoryEventStore`, take automatic snapshots at some interval into an
  `InMemorySnapshotStore`, then for **every** `v in 1..=N` assert
  `state_at(.., v) == re_hydrate(None, read_events_range(.., None, Some(v)))`. (No `proptest`
  dependency — a deterministic loop over all `v`.) Gated behind CLOUD-183 availability per
  §3.4; if interim, exercise via `read_events_since` + truncation.
- **No-config backward-compat:** an aggregate built without `SnapshottingAggregate` (default
  `after_persist`) produces the same events/state/version as the existing aggregate tests —
  reuse/extend the current `epoch_mem` aggregate tests (`epoch_mem/src/aggregate.rs:~395`)
  unchanged to prove R-7.
- **Capture + retention integration:** a `SnapshottingAggregate` with
  `Automatic { interval: 3 }` + `KeepLast(2)`: drive 10 commands; assert snapshots exist at
  the expected boundary versions and only the last two survive.

### 6.3 `epoch_pg` (DB-gated integration, `#[serial]` + `#[tokio::test]`, fresh `Uuid`s)

- New `epoch_pg/tests/pgsnapshotstore_integration_tests.rs`:
  - `test_save_and_load_nearest`: save v2/v5/v9; assert `load(7)→v5`, `load(9)→v9`,
    `load(1)→None`.
  - `test_save_snapshot_idempotent`: re-save v5; one row; latest state wins.
  - `test_keep_last_prunes`: `KeepLast(2)` leaves the two highest; `Unlimited` no-op.
  - `test_state_at_matches_full_replay` (if CLOUD-183 landed): build a PG stream, snapshot
    at an interval, assert `state_at(v)` equals full replay for several `v`.
- `epoch_pg/tests/migrations_integration_tests.rs`: add `applied_after[12]` assertions
  (`version == 13`, `name == "create_snapshots_table"`); `applied.len() == MIGRATION_COUNT`
  is already asserted and auto-updates.

### 6.4 Regression

`cargo test --workspace` green; existing suites **unmodified** (R-7, R-11);
`cargo clippy --all-targets -- -D warnings`; `cargo fmt --check`. Migration meta-tests
(order/unique versions+names) pass with m013.

---

## 7. Risks & Mitigations

- **Breaking the byte-for-byte guarantee (R-7).** The only `handle()` change is one call to
  a defaulted empty method in the state-present branch. *Mitigation:* default body is a
  no-op; the existing aggregate test suites run **unmodified** as the regression gate; the
  `delete_state` branch is untouched.
- **Snapshot failure poisoning a committed command.** Events are already durable when
  `after_persist` runs. *Mitigation:* `capture_snapshot_if_due` logs and swallows store
  errors and returns `()` — a snapshot is a rebuildable cache, never on the command's
  critical path.
- **`Automatic { interval: 0 }` divide-by-zero.** *Mitigation:* the trigger match guards
  `interval > 0`; `0` means "never". Unit-tested.
- **CLOUD-183 not landed.** `state_at` needs `read_events_range`. *Mitigation:* §3.4 —
  isolate `state_at` to the final phase; preferred path lands CLOUD-183 first, documented
  interim otherwise. Phases 1–5 are independent.
- **Migration numbering drift.** *Mitigation:* verified latest is m012 (§3.3); m013 is the
  next free version; `MIGRATION_COUNT` auto-tracks; only an `applied_after[12]` assertion is
  added.
- **`KeepLast(0)` foot-gun (empties snapshots).** *Mitigation:* documented; "keep all" is
  `Unlimited`, not `KeepLast(u32::MAX)`.
- **Large state JSONB cost.** Snapshots store full serialized state per captured version.
  *Mitigation:* opt-in; retention bounds growth; `interval` controls frequency. Noted for
  consumers.

---

## 8. Considered Alternatives

- **Extend `StateStoreBackend` with versioned methods.** Rejected — snapshots are a distinct
  concern (version-keyed history vs. single live state) and conflating them would force a
  breaking change on every existing state store (violates NG-1, G-6).
- **`state_at` as an `EventApplicator` method.** Rejected — `EventApplicator` is implemented
  by projections, which must not acquire snapshot/event-store dependencies. A free function
  composes the pieces without polluting the trait.
- **`Option<(&dyn SnapshotStore, &SnapshotConfig)>` accessor on `Aggregate`.** Rejected for
  the extension-trait approach (§4.7) due to associated-`Error`/generic-`S` trait-object
  friction.
- **Shared `epoch_core::testing::verify_snapshot_store` parity helper.** Deferred (NG-7),
  matching spec 0022 §8; can be added if a third backend appears.
- **Snapshot-fast-path inside `handle()`'s own re-hydration read.** Deferred (NG-3, OQ-1) to
  preserve the auditable byte-for-byte guarantee this iteration.

---

## 9. Breaking Change & Dependency Notes

- **Non-breaking (additive).** New trait `SnapshotStore`, new types, new `state_at`, a
  **defaulted** `Aggregate::after_persist` hook, and the `SnapshottingAggregate` extension
  trait. External `Aggregate` implementors compile unchanged (defaulted method); external
  `EventStoreBackend`/`StateStoreBackend` impls are untouched. Commit scopes: `feat(core)`,
  `feat(pg)`, `feat(mem)`, `feat(aggregate)` — **no `!`**.
- **New migration m013** (`create_snapshots_table`) — additive schema, forward-only, no
  backfill, no event-format change.
- **CLOUD-183 dependency (scoped to `state_at`).** `state_at` requires
  `EventStoreBackend::read_events_range` (spec 0022), which is **not yet in the codebase**
  (§3.4). Land/co-deliver CLOUD-183 for the target implementation, or use the documented
  interim. The rest of CLOUD-184 has no such dependency.

---

## 10. Files to Create / Modify

| File | Change |
|------|--------|
| `epoch_core/src/snapshot.rs` | **New** — `SnapshotConfig`/`SnapshotTrigger`/`SnapshotRetention`/`Snapshot<S>`, `SnapshotStore<S>`, `SnapshottingAggregate<ED>`, `state_at`, `StateAtError`, `SaveSnapshotError`; unit tests |
| `epoch_core/src/lib.rs` | Add `pub mod snapshot;`; re-export `snapshot::*` in `prelude` |
| `epoch_core/src/aggregate.rs` | Add defaulted no-op `after_persist`; call it in `handle()`'s state-present branch after `persist_state` |
| `epoch_mem/src/snapshot.rs` (or in `event_store.rs`) | **New** — `InMemorySnapshotStore<S>` + `InMemorySnapshotStoreError`; unit tests |
| `epoch_mem/src/lib.rs` | Re-export the in-memory snapshot store |
| `epoch_pg/src/migrations/m013_create_snapshots_table.rs` | **New** — `CreateSnapshotsTable` (version 13) |
| `epoch_pg/src/migrations/mod.rs` | Declare/import/append `&CreateSnapshotsTable` to `MIGRATIONS` |
| `epoch_pg/src/snapshot_store.rs` | **New** — `PgSnapshotStore<S>` |
| `epoch_pg/src/lib.rs` | Re-export `PgSnapshotStore` |
| `epoch_pg/tests/migrations_integration_tests.rs` | Add `applied_after[12]` (version 13, `create_snapshots_table`) assertions |
| `epoch_pg/tests/pgsnapshotstore_integration_tests.rs` | **New** — §6.3 integration tests |
| `CHANGELOG.md` | Unreleased `Added` entry: versioned snapshot store, m013, `state_at`, opt-in aggregate capture/prune (note CLOUD-183 dependency for `state_at`) |

No new dependencies (`async_trait`, `serde`, `serde_json`, `sqlx`, `uuid`, `log`, `tokio`,
`thiserror` already present in the relevant crates).

---

## 11. Phased Delivery Plan

Each phase follows TDD (red → green → refactor). Phases 1–5 are independent of CLOUD-183;
Phase 6 (`state_at`) carries the CLOUD-183 dependency and is intentionally last so it can be
deferred without blocking the rest.

### Phase 1 — `epoch_core` config + `SnapshotStore` trait

- **Goal:** `snapshot.rs` with `SnapshotConfig`/`SnapshotTrigger`/`SnapshotRetention`/
  `Snapshot<S>` and the `SnapshotStore<S>` trait; `lib.rs` module + prelude wiring; unit
  tests for the value types.
- **Out of bounds:** no aggregate change, no backends, no `state_at` yet.
- **Exit:** `cargo test -p epoch_core` green; `#![deny(missing_docs)]` satisfied.
- **Effort:** S. **Difficulty:** standard. **Requirements:** R-1, R-2.

### Phase 2 — `epoch_mem::InMemorySnapshotStore`

- **Goal:** Implement + re-export `InMemorySnapshotStore<S>`; unit tests (nearest-`≤`,
  idempotent save, `KeepLast`/`Unlimited` retention).
- **Exit:** `cargo test -p epoch_mem` green.
- **Effort:** S. **Difficulty:** standard. **Requirements:** R-3, R-10 (partial).

### Phase 3 — `epoch_pg` migration m013 + `PgSnapshotStore`

- **Goal:** Migration m013 `create_snapshots_table` + registry + `applied_after[12]` test
  assertion; `PgSnapshotStore<S>` with §4.5 SQL; re-export; integration tests
  (`pgsnapshotstore_integration_tests.rs`).
- **Out of bounds:** no `epoch_events` changes; no rename guard (new table).
- **Exit:** migration meta-tests + `pgsnapshotstore` integration suite pass on a running PG;
  fresh DB shows `epoch_snapshots`.
- **Effort:** M. **Difficulty:** standard. **Requirements:** R-4, R-5, R-10 (partial).

### Phase 4 — `Aggregate` no-op hook (backward-compat gate)

- **Goal:** Add the defaulted no-op `after_persist` to `Aggregate`; wire the single call
  into `handle()`'s state-present branch.
- **Out of bounds:** no snapshot logic here — the default body is empty.
- **Exit (HARD):** the **existing aggregate test suites pass unmodified**; an aggregate with
  no override performs **zero extra I/O** and is **byte-for-byte identical** to today
  (R-7). This is the explicit exit criterion because this is the only phase that touches
  `Aggregate::handle()`.
- **Effort:** S. **Difficulty:** hard (correctness of the no-regression guarantee).
  **Requirements:** R-6, R-7.

### Phase 5 — `SnapshottingAggregate` capture + prune + manual `save_snapshot`

- **Goal:** Extension trait with `capture_snapshot_if_due` (boundary-cross capture, retention
  prune, errors logged-not-fatal) and `save_snapshot(id)`; `SaveSnapshotError`. Tests:
  interval honoured, `KeepLast(n)` prunes, manual path, `interval == 0` never captures.
- **Exit (HARD, repeats the constraint):** an aggregate that does **not** implement
  `SnapshottingAggregate` is still byte-for-byte identical (the default `after_persist`
  no-op is unchanged); existing suites unmodified (R-7).
- **Effort:** M. **Difficulty:** hard (capture arithmetic + non-fatal error discipline).
  **Requirements:** R-8, R-10 (partial).

### Phase 6 — `state_at` helper (CLOUD-183-dependent)

- **Goal:** `state_at` + `StateAtError`; deterministic equivalence test
  (`state_at(v) == replay-from-zero` for all `v`).
- **Dependency:** `read_events_range` (CLOUD-183, §3.4). Preferred: land/co-deliver
  CLOUD-183; interim: `read_events_since` + truncate with `TODO(CLOUD-183)`.
- **Exit:** equivalence tests green for both backends (mem unit; pg integration if DB
  available).
- **Effort:** M. **Difficulty:** standard (given CLOUD-183). **Requirements:** R-9, R-10.

### Phase 7 — Hygiene & docs

- **Goal:** `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, full `cargo test`;
  CHANGELOG `Added` entry (note the CLOUD-183 dependency for `state_at`).
- **Exit (acceptance):** R-11 gates green; existing suite unmodified.
- **Effort:** S. **Difficulty:** standard. **Requirements:** R-11.

### 11.1 Machine-readable phases

```json
{
  "issue": "CLOUD-184",
  "spec": "specs/0023-cloud184-versioned-snapshot-store.md",
  "depends_on_specs": ["specs/0022-cloud183-read-events-range.md"],
  "depends_on_status": "CLOUD-183 (read_events_range) NOT yet in codebase as of 2026-06-21; required only by Phase 6 (state_at)",
  "phases": [
    { "phase": 1, "focus": "epoch_core: snapshot.rs with SnapshotConfig/Trigger/Retention, Snapshot<S>, SnapshotStore<S> trait; lib.rs mod + prelude", "effort": "S", "difficulty": "standard", "requirements": ["R-1","R-2"], "depends_on": [] },
    { "phase": 2, "focus": "epoch_mem: InMemorySnapshotStore<S> (nearest-<=, idempotent save, KeepLast/Unlimited) + re-export + unit tests", "effort": "S", "difficulty": "standard", "requirements": ["R-3","R-10"], "depends_on": [1] },
    { "phase": 3, "focus": "epoch_pg: migration m013 create_snapshots_table (version 13) + registry + applied_after[12] test; PgSnapshotStore<S> + integration tests", "effort": "M", "difficulty": "standard", "requirements": ["R-4","R-5","R-10"], "depends_on": [1] },
    { "phase": 4, "focus": "epoch_core aggregate: defaulted no-op after_persist hook + single call site in handle() state-present branch; BYTE-FOR-BYTE backward-compat exit gate", "effort": "S", "difficulty": "hard", "requirements": ["R-6","R-7"], "depends_on": [1] },
    { "phase": 5, "focus": "SnapshottingAggregate extension trait: capture_snapshot_if_due (boundary capture + retention prune, non-fatal) + manual save_snapshot; SaveSnapshotError; reaffirm no-config byte-for-byte identical", "effort": "M", "difficulty": "hard", "requirements": ["R-8","R-7","R-10"], "depends_on": [1,2,4] },
    { "phase": 6, "focus": "state_at free fn + StateAtError; deterministic state_at(v)==replay-from-zero equivalence tests. DEPENDS on CLOUD-183 read_events_range (interim: read_events_since + truncate)", "effort": "M", "difficulty": "standard", "requirements": ["R-9","R-10"], "depends_on": [1,2,3], "external_dependency": "CLOUD-183" },
    { "phase": 7, "focus": "Hygiene: cargo fmt, clippy --all-targets -D warnings, full cargo test; CHANGELOG Added entry", "effort": "S", "difficulty": "standard", "requirements": ["R-11"], "depends_on": [1,2,3,4,5,6] }
  ]
}
```

---

## 12. Acceptance Criteria (mapped to CLOUD-184)

| ID | Criterion | Ticket scope |
|----|-----------|--------------|
| AC-1 | `SnapshotStore<S>` + `SnapshotConfig`/`SnapshotTrigger`/`SnapshotRetention`/`Snapshot<S>` exist in `epoch_core`; first-iteration options present; enums extensible (`#[non_exhaustive]`) | #1, #2 |
| AC-2 | `PgSnapshotStore` + migration **m013** (`epoch_snapshots` with `(stream_id, version)` index) | #3 |
| AC-3 | `InMemorySnapshotStore` for tests | #4 |
| AC-4 | `Aggregate::handle()` takes automatic snapshots when `Automatic { interval }` is configured and prunes per `retention` | #5 |
| AC-5 | `Aggregate::save_snapshot(id)` supports the `Manual` path | #6 |
| AC-6 | `state_at(version)` returns state equivalent to a full replay from zero | #7 |
| AC-7 | Aggregates with no snapshot config behave **byte-for-byte identically** (no perf regression, no behaviour change) | #8 (hard constraint) |
| AC-8 | `cargo clippy -- -D warnings` clean; existing test suite passes **unmodified** | #9 |

---

## 13. Open Questions

- **OQ-1 (snapshot fast-path in `handle()`'s own read):** Should `handle()`'s re-hydration
  read itself prefer the nearest snapshot? **Deferred** (NG-3) — this iteration keeps
  `handle()`'s hot read unchanged to preserve the auditable byte-for-byte guarantee; the
  fast-path is exposed via `state_at`. Revisit once snapshotting has bedded in.
- **OQ-2 (CLOUD-183 sequencing):** Land CLOUD-183 before Phase 6, or ship Phase 6 with the
  `read_events_since` + truncate interim? **Recommend** landing/co-delivering CLOUD-183
  (both must precede sentinel's `evaluate_at` anyway); interim is a documented bridge only.
- **OQ-3 (`SnapshotConfig` default):** No `Default` is derived (no obviously-correct default
  trigger). Confirm consumers always construct explicitly. **Recommend** no default.
- **OQ-4 (custom snapshot table name):** `PgSnapshotStore` uses the fixed `epoch_snapshots`
  (NG-4). Add a `with_table` variant later if a consumer needs multiple snapshot tables.
</content>

</invoke>
