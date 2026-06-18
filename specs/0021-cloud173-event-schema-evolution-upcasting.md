# Spec 0021: `epoch` — Event Schema Evolution / Upcasting Mechanism

**Issue:** Linear [CLOUD-173](https://linear.app/catallactical/issue/CLOUD-173/epoch-event-schema-evolution-upcasting-mechanism-serdedefault-patches-wont-scale)
**Title:** epoch: event schema evolution / upcasting mechanism (`serde(default)` patches won't scale)
**Status:** Proposed (ready for review)
**Created:** 2026-06-18
**Ticket opened:** 2026-06-10 (Triage, Priority 2)
**Crates:** `epoch_core` (mechanism), `epoch_derive` (version metadata), `epoch_pg` (deserialization boundary + migration), `epoch_mem` (parity tests), `epoch` (docs/guide)
**Commit scope:** `feat(core)!` / concept scope `event-store`
**Target implementation:** `/home/istar/code/epoch`
**Workspace version:** `0.1.0` (pre-1.0 — breaking changes permitted with a clear migration note)
**Prior art:** Axon upcasters, Marten event versioning, EventStoreDB event migration.

---

## 1. Problem Statement

Epoch has **no first-class story for event schema evolution**. Once an event variant is
persisted, its serialized shape is frozen in the store forever, yet the in-memory
`EventData` type that it must deserialize into keeps changing as the domain evolves.
Today the only tool available to bridge that gap is ad-hoc serde annotation.

### 1.1 Current evidence

- The payload trait `EventData` (`epoch_core/src/event.rs`) is defined as
  `Serialize + DeserializeOwned + Sized + Clone + Send` and exposes a single
  `event_type(&self) -> &'static str`. There is **no notion of a payload/schema
  version** anywhere in the envelope.
- The `Event<D>` envelope (`epoch_core/src/event.rs`) carries `event_type: String` and
  `data: Option<D>` but **no `schema_version`**.
- The only evolution technique in use is `#[serde(default)]` on newly-added optional
  fields inside event enums. This works for **purely additive, optional** fields and
  nothing else. It cannot express: a renamed field, a removed/required field, a split
  or merged variant, a changed field type, or a renamed variant.
- The Postgres read path (`epoch_pg/src/event_store.rs::pg_db_event_to_event`)
  deserializes a stored `serde_json::Value` straight into the *current* `D` via
  `serde_json::from_value`. A structural mismatch surfaces as
  `PgEventStoreError::DeserializeEventError`, which **propagates up the event stream and
  aborts rehydration** for the entire stream/projection — a hard stop, not a recovery.
- To avoid that hard stop, downstream code has resorted to "skip-with-log" catch-alls
  (e.g. `#[serde(other)]` fallback variants or filtering deserialization errors during
  replay). This converts a hard failure into **silent event loss**: an event that no
  longer deserializes is dropped, the projection silently diverges from the true
  history, and nothing fails loudly.

### 1.2 Why `serde(default)` patches will not scale

`#[serde(default)]` is a per-field, additive-only escape hatch with no version
awareness. Every non-additive change (rename, type change, variant restructuring,
field removal) has **no expressible migration** and forces teams into one of two bad
outcomes:

1. **Hard failure** — replay/rehydration aborts on the first un-deserializable historic
   event (current `epoch_pg` behavior), or
2. **Silent loss** — a catch-all swallows the event and the read model is quietly wrong.

Neither is acceptable for an auditable event-sourced system. We need an explicit,
versioned, testable **upcasting** mechanism that transforms historic serialized
payloads forward to the current schema *before* they are deserialized into `D`, with
**fail-loud-by-default** semantics and observable, opt-in degradation.

---

## 2. Goals and Outcomes

### 2.1 Goals

- **G-1** Introduce an explicit, versioned **upcaster mechanism**: stored payloads are
  transformed forward, version by version, to the current schema before deserialization
  into the domain `EventData` type.
- **G-2** Add **schema version metadata** to the event envelope and to persisted rows,
  with a default that makes pre-existing (un-versioned) events well-defined.
- **G-3** Define **explicit, configurable failure semantics** for deserialization /
  upcasting that default to **fail-loud** and make any lossy behavior an *explicit,
  observable* opt-in — eliminating accidental silent event loss.
- **G-4** Provide **observability** (structured logging, metrics counters, and an
  optional dead-letter capture) for upcasting outcomes.
- **G-5** Ship a **forward-only Postgres migration** that adds the version column to the
  existing `epoch_events` table without backfilling or rewriting, consistent with the
  established migration conventions.
- **G-6** Provide **design guidance documentation** for schema evolution (what each kind
  of change requires, when to upcast vs. add a new event type, testing recipes).
- **G-7** Be **opt-in and zero-cost when unused**: projects that do not register any
  upcaster behave exactly as today (modulo the new metadata column), and `epoch_core`'s
  default dependency footprint is unchanged.

### 2.2 Outcomes / Definition of Done

- A domain team can rename a field, remove a field, or restructure a variant, register a
  single upcaster step, and have every historic event replay correctly into the new
  schema — verified by a test.
- A deserialization/upcasting failure **cannot** silently drop an event. It either
  aborts loudly (default) or is routed to an explicit, counted, logged dead-letter sink
  (opt-in).
- Existing databases continue to boot and replay after the migration with **no backfill
  step** and no behavioral change for projects that register no upcasters.

### 2.3 Non-Goals

- **NG-1** **Downcasting** (transforming new events backward for old consumers). Epoch
  is forward-only; out of scope.
- **NG-2** **Rewriting / mutating stored events in place.** Upcasting happens on read;
  the event log remains append-only and immutable (consistent with the forward-only,
  no-`down()` migration philosophy in `epoch_pg/src/migrations/mod.rs`).
- **NG-3** A bulk "re-stamp historic rows with a schema version" backfill job. Legacy
  rows are interpreted via a well-defined default version (see §6).
- **NG-4** Changing the `Serialize`/`DeserializeOwned` bound on `EventData` or the
  serialization format. JSON-via-serde remains the wire format.
- **NG-5** Cross-event-type upcasting (one stored type becoming a different type). The
  initial mechanism upcasts within a single logical `event_type`. Splitting/merging
  event *types* is documented as a "new event type + projection rebuild" pattern in the
  guidance doc, not a code feature in this spec.

---

## 3. Background: How Deserialization Works Today

| Backend | Storage of payload | Deserialization boundary |
|---|---|---|
| `epoch_mem` | Stores the **typed** `Event<D>` in memory; no serde round-trip on read. | None — no upcasting needed at read time. |
| `epoch_pg` | Stores `data` as `JSONB` (`serde_json::Value`); `event_type` as `VARCHAR`. | `pg_db_event_to_event` calls `serde_json::from_value::<D>` — **the single choke point** where historic bytes meet the current type. |

This asymmetry matters: **upcasting is only meaningful at a serialized boundary**, which
in the first-party backends means `epoch_pg`. `epoch_core` owns the *mechanism* (the
trait, registry, version metadata, failure policy); `epoch_pg` *invokes* it at its read
path. `epoch_mem` gains parity tests by round-tripping through the registry explicitly.

`epoch_core` currently depends on `serde` but **not** `serde_json`. The intermediate
representation an upcaster mutates is JSON-shaped (matching the only persistent
backend). We therefore gate the mechanism behind an opt-in `upcasting` feature that
pulls in an optional `serde_json` dependency, preserving G-7 (zero-cost when unused, no
new mandatory deps).

---

## 4. Design Overview

### 4.1 Conceptual model

```
stored row:  { event_type: "OrderPlaced", schema_version: 1, data: <json v1> }
                                   │
                                   ▼
        ┌─────────────────────────────────────────────┐
        │  UpcasterRegistry.upcast(event_type, from=1) │   apply step 1→2, 2→3, …
        └─────────────────────────────────────────────┘
                                   │  json upcast to current version (3)
                                   ▼
                serde_json::from_value::<D>(json_v3)  ──►  Event<D>
                                   │ on error
                                   ▼
                        FailurePolicy  (Fail | DeadLetter)
```

An **upcaster** is a single, version-pinned transformation `(event_type, from_version)
→ json at from_version+1`. A **registry** holds the ordered chain per `event_type` and
applies steps until the payload reaches the type's *current* version, then hands off to
serde. Chaining keeps each step small and independently testable (Axon-style).

### 4.2 New `epoch_core` API (feature `upcasting`)

```rust
// epoch_core/src/upcasting.rs  (new, behind `#[cfg(feature = "upcasting")]`)

use serde_json::Value;

/// The schema version of an event payload. `1` is the implicit version of any
/// event written before versioning existed (see §6 default-version rule).
pub type SchemaVersion = u32;

/// Context handed to an upcaster step: the immutable envelope metadata an
/// upcaster may need to make a decision (but must not mutate).
#[non_exhaustive]
pub struct UpcastContext<'a> {
    pub event_type: &'a str,
    pub from_version: SchemaVersion,
    pub stream_id: uuid::Uuid,
    pub event_id: uuid::Uuid,
}

/// A single forward transformation of one event_type's payload from
/// `from_version()` to `from_version() + 1`.
pub trait Upcaster: Send + Sync {
    /// The `event_type` (PascalCase, matching `EventData::event_type`) this step applies to.
    fn event_type(&self) -> &str;
    /// The version this step upgrades *from*. It always produces `from_version() + 1`.
    fn from_version(&self) -> SchemaVersion;
    /// Transform the JSON payload one version forward. Must be pure and deterministic.
    fn upcast(&self, ctx: &UpcastContext<'_>, payload: Value) -> Result<Value, UpcastError>;
}

/// Outcome policy applied when the *final* deserialization (after all upcast steps)
/// fails, or when an upcaster step itself errors.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FailurePolicy {
    /// Abort the read/replay with a typed error. Default — never lose an event silently.
    #[default]
    Fail,
    /// Route the offending event to the registered dead-letter sink, increment the
    /// `upcast_dead_lettered` counter, log at ERROR, and *continue* the stream.
    /// This is the ONLY way to skip an event, and it is explicit, counted, and captured.
    DeadLetter,
}

#[derive(Debug, thiserror::Error)]
pub enum UpcastError {
    #[error("no upcaster registered for {event_type} at version {from_version}, \
             but current version is {current_version}")]
    MissingStep { event_type: String, from_version: SchemaVersion, current_version: SchemaVersion },
    #[error("upcaster for {event_type} v{from_version} failed: {source}")]
    Step { event_type: String, from_version: SchemaVersion,
           #[source] source: Box<dyn std::error::Error + Send + Sync> },
    #[error("stored schema_version {stored} is newer than current {current} for {event_type} \
             (binary is older than the data it is reading)")]
    FutureVersion { event_type: String, stored: SchemaVersion, current: SchemaVersion },
}

/// Captures an event that could not be upcast/deserialized, for the DeadLetter policy.
#[non_exhaustive]
pub struct DeadLetteredEvent {
    pub event_id: uuid::Uuid,
    pub stream_id: uuid::Uuid,
    pub event_type: String,
    pub stored_version: SchemaVersion,
    pub raw_payload: serde_json::Value,
    pub reason: String,
}

#[async_trait::async_trait]
pub trait DeadLetterSink: Send + Sync {
    async fn capture(&self, event: DeadLetteredEvent);
}

/// Holds the per-`event_type` upcaster chains, the per-type current version, the
/// failure policy, and an optional dead-letter sink.
#[derive(Default)]
pub struct UpcasterRegistry { /* … */ }

impl UpcasterRegistry {
    pub fn new() -> Self;
    pub fn register(&mut self, upcaster: impl Upcaster + 'static) -> &mut Self;
    pub fn with_policy(&mut self, policy: FailurePolicy) -> &mut Self;
    pub fn with_dead_letter_sink(&mut self, sink: impl DeadLetterSink + 'static) -> &mut Self;

    /// Apply the chain for `event_type` starting at `from_version` until the current
    /// version, returning the upcast JSON ready for `from_value::<D>`.
    pub fn upcast(&self, ctx: &UpcastContext<'_>, payload: Value) -> Result<Value, UpcastError>;

    /// One-call convenience used by backends: upcast then deserialize into `D`,
    /// applying the configured `FailurePolicy`. Returns `Ok(None)` ONLY when an event
    /// was explicitly dead-lettered (so the backend skips exactly that row).
    pub fn upcast_and_deserialize<D: EventData>(
        &self, event_type: &str, stored_version: SchemaVersion,
        stream_id: Uuid, event_id: Uuid, payload: Option<Value>,
    ) -> Result<Option<D>, UpcastError>;
}
```

### 4.3 Version metadata on the envelope and trait

- `EventData` gains a defaulted method:

  ```rust
  /// Current schema version of this payload type. Defaults to 1.
  /// Bump this when you register an upcaster that targets a new version.
  fn schema_version(&self) -> SchemaVersion { 1 }
  ```

  Defaulted so it is **non-breaking for existing `EventData` impls** (and for the derive
  macro consumers). The `#[derive(EventData)]` macro is extended to accept an optional
  `#[event_data(schema_version = N)]` attribute that overrides the default for the whole
  enum.

- `Event<D>` and `EventBuilder<D>` gain a `schema_version: SchemaVersion` field (default
  `1`). It is populated from `EventData::schema_version()` at construction (write path)
  and from the stored column at read (read path). This mirrors how `global_sequence` /
  causation fields were threaded through the builder in earlier specs.

### 4.4 Wiring the Postgres read path

`PgEventStore` gains an optional `Arc<UpcasterRegistry>` (constructor
`with_upcasters(...)`; the existing constructors set an empty registry whose behavior is
identical to today for any event that still deserializes). `pg_db_event_to_event` is
changed from:

```rust
let data: Option<D> = entry.data.map(serde_json::from_value).transpose()
    .map_err(PgEventStoreError::DeserializeEventError)?;
```

to route through the registry:

```rust
let data: Option<D> = registry.upcast_and_deserialize::<D>(
    &entry.event_type, entry.schema_version.unwrap_or(1) as u32,
    entry.stream_id, entry.id, entry.data,
)?; // Ok(None) only on explicit dead-letter → row is skipped, counted, logged.
```

`PgDBEvent` gains `#[sqlx(default)] schema_version: Option<i32>` (NULL → treated as
version 1, see §6). All `SELECT`/`INSERT` column lists in `epoch_pg/src/event_store.rs`
are extended to include `schema_version`.

### 4.5 In-memory backend

`epoch_mem` stores typed events and performs no read-time deserialization, so it needs
no registry wiring. It gains tests that exercise `UpcasterRegistry` directly (JSON in →
JSON out → `from_value`) to guarantee the mechanism is backend-independent.

---

## 5. Requirements (numbered, traceable)

### 5.1 Upcaster mechanism (epoch_core)

- **R-1** `epoch_core` SHALL define an `Upcaster` trait representing a single
  `(event_type, from_version) → from_version+1` JSON transformation. *(G-1)*
- **R-2** `epoch_core` SHALL define an `UpcasterRegistry` that stores ordered upcaster
  chains keyed by `event_type`, tracks each type's current version, and applies steps in
  ascending version order until the current version is reached. *(G-1)*
- **R-3** The registry SHALL detect and reject a **missing step** in a chain
  (`UpcastError::MissingStep`) rather than silently leaving the payload at an
  intermediate version. *(G-1, G-3)*
- **R-4** The registry SHALL detect a **future version** (stored version > current
  version, i.e. an older binary reading newer data) and return
  `UpcastError::FutureVersion`. *(G-3)*
- **R-5** Upcaster steps SHALL be **pure and deterministic** (documented contract);
  re-running a replay MUST yield identical results (supports CLAUDE.md idempotency/
  repeatability testing guidance). *(G-1)*
- **R-6** The entire mechanism SHALL live behind an opt-in `upcasting` cargo feature
  that adds an **optional** `serde_json` dependency to `epoch_core`; with the feature
  off, `epoch_core`'s dependency set and public API are unchanged. *(G-7)*

### 5.2 Schema version metadata

- **R-7** `EventData` SHALL gain a `schema_version(&self) -> SchemaVersion` method
  defaulting to `1`, so all existing impls compile unchanged. *(G-2, G-7)*
- **R-8** `Event<D>` and `EventBuilder<D>` SHALL carry a `schema_version` field
  (default `1`), populated from `EventData::schema_version()` on the write path and
  preserved through `to_subset_event`, `to_subset_event_ref`, `to_superset_event`,
  `into_builder`, and `build`. *(G-2)*
- **R-9** `#[derive(EventData)]` SHALL accept an optional
  `#[event_data(schema_version = N)]` enum-level attribute that overrides the default;
  absence yields version `1`. *(G-2)*

### 5.3 Failure handling and observability

- **R-10** A `FailurePolicy` SHALL be configurable on the registry, defaulting to
  `Fail` (abort the read/replay with a typed error). *(G-3)*
- **R-11** Silent event loss SHALL be impossible: the **only** path that skips an event
  is the explicit `FailurePolicy::DeadLetter`, which MUST (a) capture the raw event via a
  `DeadLetterSink`, (b) increment an `upcast_dead_lettered` counter, and (c) log at
  `ERROR` with `event_id`, `event_type`, stored/current versions, and reason. *(G-3, G-4)*
- **R-12** The registry SHALL emit structured logs/metrics for upcasting outcomes:
  counters for `upcast_applied` (per step), `upcast_succeeded`, `upcast_failed`, and
  `upcast_dead_lettered`; DEBUG logs per applied step; ERROR logs on failure. *(G-4)*
- **R-13** When `FailurePolicy::Fail` is active, an upcasting/deserialization error in
  `epoch_pg` SHALL surface as a typed `PgEventStoreError` variant
  (`UpcastError`) that propagates through the event stream exactly as a hard error does
  today — never swallowed. *(G-3)*

### 5.4 Migration for version metadata

- **R-14** A new forward-only migration (`m012_add_schema_version_to_events`) SHALL add a
  **nullable** `schema_version INT` column to `epoch_events` using
  `ADD COLUMN IF NOT EXISTS`, with **no backfill** and **no table rewrite**, following
  the pattern established by `m011_add_txid_to_events`. *(G-5, NG-3)*
- **R-15** The migration SHALL set a column `DEFAULT 1` so that new inserts that do not
  explicitly supply a version are stamped `1`, while pre-migration rows remain `NULL`
  and are interpreted as version `1` by the read path (§6). *(G-5)*
- **R-16** The migration SHALL include the same **table-rename guard** as m011 (no-op
  with a WARN if `epoch_events` does not exist post-cutover), and custom tables created
  via `PgEventStore::with_table` SHALL receive the column via an idempotent
  `ensure_schema_version_column` helper. *(G-5)*
- **R-17** The migration MUST be registered in `epoch_pg/src/migrations/mod.rs`
  `MIGRATIONS` array (append-only, version 12). *(G-5)*

### 5.5 Design guidance documentation

- **R-18** `docs/guide.md` (or a new `docs/schema-evolution.md` linked from it) SHALL
  document: which change kinds are additive-safe (`#[serde(default)]`) vs. require an
  upcaster; how to register an upcaster and bump `schema_version`; the
  fail-vs-dead-letter policy trade-off; and the "new event type + rebuild" pattern for
  type splits/merges (NG-5). *(G-6)*
- **R-19** The guidance SHALL include a worked example covering at least: (a) add
  optional field, (b) rename field, (c) remove field, (d) change field type, (e) add a
  new required field with an upcaster-supplied default. *(G-6)*

### 5.6 Validation / testing

- **R-20** `epoch_core` SHALL ship a feature-gated, reusable contract test
  (`epoch_core::testing::verify_upcasting_chain`) that any backend can run, asserting a
  multi-step chain upcasts a v1 payload to current and deserializes correctly, mirroring
  the `verify_store_events_atomicity` pattern. *(G-1, G-6)*
- **R-21** Tests SHALL prove: missing-step detection (R-3), future-version detection
  (R-4), `Fail` aborts loudly (R-10/R-13), `DeadLetter` captures + counts + continues and
  **never** loses an event silently (R-11). *(G-3)*
- **R-22** A DB-gated `epoch_pg` integration test SHALL store a v1 JSON payload directly,
  register a 1→2 upcaster, bump the type's `schema_version` to 2, and assert
  rehydration/replay yields the v2-shaped state. *(G-1, G-5)*
- **R-23** `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo test` (workspace; DB-gated tests where a database is available, with both the
  `upcasting` feature on and off) SHALL be clean. *(quality gate)*

---

## 6. Default-Version Rule for Legacy Rows

Pre-migration rows have `schema_version = NULL`. The read path interprets **`NULL` as
version `1`** (`entry.schema_version.unwrap_or(1)`). This is the linchpin that lets us
avoid a backfill (NG-3):

- A project that has **never** changed an event shape keeps every type at version `1`,
  registers no upcasters, and replays exactly as today.
- A project that introduces its **first** breaking change to `OrderPlaced` registers a
  `1 → 2` upcaster and sets `OrderPlaced`'s `schema_version()` to `2`. Every historic row
  (NULL → treated as 1, or explicitly 1) flows through the `1 → 2` step; new rows are
  written stamped `2` and skip the step.

Because `1` is the universal floor, legacy NULL rows and explicitly-`1` rows are
indistinguishable to the chain — which is exactly correct.

---

## 7. Proposed Changes (files touched)

| File | Change |
|------|--------|
| `epoch_core/Cargo.toml` | Add `[features] upcasting = ["dep:serde_json"]`; `serde_json` as optional dep |
| `epoch_core/src/upcasting.rs` | **New** — `Upcaster`, `UpcasterRegistry`, `FailurePolicy`, `UpcastError`, `UpcastContext`, `DeadLetterSink`, `DeadLetteredEvent`, `SchemaVersion` |
| `epoch_core/src/lib.rs` | `#[cfg(feature = "upcasting")] pub mod upcasting;` + prelude re-exports (feature-gated) |
| `epoch_core/src/event.rs` | Add `EventData::schema_version()` (default `1`); add `schema_version` field to `Event`/`EventBuilder`; thread through all conversions & `build()` |
| `epoch_core/src/testing.rs` | Add feature-gated `verify_upcasting_chain` contract helper |
| `epoch_derive/src/event_data.rs` | Parse optional `#[event_data(schema_version = N)]`; emit `schema_version()` override |
| `epoch_pg/Cargo.toml` | Enable `epoch_core/upcasting` feature |
| `epoch_pg/src/event_store.rs` | Add `schema_version` to `PgDBEvent` + all SQL column lists; add `Arc<UpcasterRegistry>` to `PgEventStore` + `with_upcasters`; route `pg_db_event_to_event` through registry; new `PgEventStoreError::Upcast` variant |
| `epoch_pg/src/event_bus/mod.rs` | Add `ensure_schema_version_column` helper (mirror `ensure_txid_column`) for custom tables |
| `epoch_pg/src/migrations/m012_add_schema_version_to_events.rs` | **New** — nullable `schema_version INT DEFAULT 1`, table-rename guard, no backfill |
| `epoch_pg/src/migrations/mod.rs` | Register `AddSchemaVersionToEvents` (version 12) |
| `epoch_pg/tests/upcasting_integration_tests.rs` | **New** — DB-gated end-to-end upcast replay test |
| `epoch_mem/tests/upcasting_tests.rs` | **New** — registry-direct parity tests |
| `docs/schema-evolution.md` (+ link from `docs/guide.md`) | **New** — design guidance (R-18/R-19) |
| `CHANGELOG.md` | Breaking-change + feature entry |

---

## 8. Failure Semantics Summary

| Situation | `FailurePolicy::Fail` (default) | `FailurePolicy::DeadLetter` (opt-in) |
|---|---|---|
| Missing upcaster step in chain | `Err(UpcastError::MissingStep)` propagates; replay aborts | Dead-letter the event, count, ERROR-log, continue |
| Upcaster step returns error | `Err(UpcastError::Step)` propagates | Dead-letter, count, log, continue |
| Final `from_value::<D>` fails | `Err(PgEventStoreError::Upcast)` propagates | Dead-letter, count, log, continue |
| Stored version > current version | `Err(UpcastError::FutureVersion)` propagates | `Err(UpcastError::FutureVersion)` **still propagates** — a future version is an operator/deploy error, never silently skipped |
| Payload deserializes cleanly | `Ok(Some(D))` | `Ok(Some(D))` |

The matrix encodes G-3: there is exactly one skip path (`DeadLetter`), it is explicit,
counted, logged, and captured; everything else fails loudly. `FutureVersion` is never
skippable even under `DeadLetter`, because it indicates a binary older than its data and
must page an operator rather than quietly discard events.

---

## 9. TDD Implementation Plan

Following the repository TDD convention (red → green → refactor):

1. **(red)** Add `epoch_core/src/upcasting.rs` skeleton + `upcasting` feature. Write unit
   tests for `UpcasterRegistry`: single step, multi-step chain, missing step (R-3),
   future version (R-4), `Fail` vs `DeadLetter` (R-10/R-11). Tests fail to compile/pass.
2. **(green)** Implement the registry, chain application, error types, and policy until
   the unit tests pass.
3. **(red→green)** Add `schema_version` to `EventData` (defaulted), `Event`,
   `EventBuilder`; extend the conversion/`build` tests in `event.rs` to assert it is
   preserved (mirror the existing `*_preserves_global_sequence` tests).
4. **(red→green)** Extend `#[derive(EventData)]` for `#[event_data(schema_version = N)]`;
   add `epoch_derive` compile-pass coverage.
5. **(red→green)** Add `verify_upcasting_chain` to `epoch_core::testing`; wire the
   `epoch_mem` parity test (R-20).
6. **(red→green)** `epoch_pg`: add migration m012 + register it; add `schema_version` to
   `PgDBEvent`/SQL; add `with_upcasters` + route `pg_db_event_to_event`; add the
   `Upcast` error variant. Migration test asserts column exists & defaults; rename-guard
   no-op test (mirror m011 tests).
7. **(red→green)** DB-gated `epoch_pg` end-to-end upcast replay test (R-22).
8. **(refactor)** `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, full
   `cargo test` with the `upcasting` feature both **on and off** (R-23).
9. **Docs:** Author `docs/schema-evolution.md` (R-18/R-19); CHANGELOG breaking-change +
   migration note.

---

## 10. Acceptance Criteria

- [ ] **AC-1** `epoch_core` exposes `Upcaster` + `UpcasterRegistry` (feature `upcasting`)
  that applies ordered chains, detects missing steps and future versions. *(R-1..R-4)*
- [ ] **AC-2** `EventData::schema_version()` exists (default `1`); `Event`/`EventBuilder`
  carry and preserve `schema_version` across all conversions; the `EventData` derive
  honors `#[event_data(schema_version = N)]`. *(R-7..R-9)*
- [ ] **AC-3** `FailurePolicy` defaults to `Fail`; the only skip path is `DeadLetter`,
  which captures, counts, and logs; no code path drops an event silently;
  `FutureVersion` is never skippable. *(R-10..R-13)*
- [ ] **AC-4** Migration m012 adds nullable `schema_version INT DEFAULT 1` with the
  rename guard, no backfill, registered at version 12; custom tables get the column via
  `ensure_schema_version_column`. *(R-14..R-17)*
- [ ] **AC-5** `epoch_pg` routes its read path through the registry; a registered 1→2
  upcaster makes historic (NULL/v1) rows replay into the v2 schema in a DB-gated test.
  *(R-22)*
- [ ] **AC-6** `epoch_core::testing::verify_upcasting_chain` exists and is run by
  `epoch_mem`; failure-semantics tests (R-21) pass. *(R-20, R-21)*
- [ ] **AC-7** `docs/schema-evolution.md` documents the change-kind matrix, upcaster
  recipe, policy trade-off, and type-split pattern with worked examples. *(R-18, R-19)*
- [ ] **AC-8** `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and
  `cargo test` are clean with the `upcasting` feature **on and off**. *(R-23)*

---

## 11. Breaking Change & Migration Notes

`feat(core)!: add event schema versioning + upcasting mechanism`

The change is **source-compatible for existing `EventData` impls** (the new
`schema_version()` method is defaulted) and for the in-memory backend. The breaking
surface is:

1. **`Event<D>` / `EventBuilder<D>` gain a `schema_version` field.** Any code that
   constructs `Event`/`EventBuilder` via the **struct literal** (rather than the builder)
   must add `schema_version: 1`. Builder users are unaffected (`build()` defaults it).
2. **Postgres schema migration m012** must be applied (`Migrator::run`). It is
   forward-only, metadata-only, no backfill, and rename-safe.

**Operational migration steps for downstream:**

- Run pending migrations (`Migrator::run`) — adds `schema_version` (NULL on legacy rows,
  interpreted as `1`).
- No backfill required; no replay required for projects that register no upcasters.
- To evolve an event non-additively: register a `from → from+1` upcaster on a
  `UpcasterRegistry`, pass it to `PgEventStore::with_upcasters`, and bump that event
  type's `schema_version()`.
- Choose a `FailurePolicy`; default `Fail` is recommended. Use `DeadLetter` only with a
  real sink and alerting on the `upcast_dead_lettered` counter.

---

## Phases (JSON)

```json
{
  "phases": [
    { "phase": 1, "focus": "epoch_core upcasting mechanism: Upcaster trait, UpcasterRegistry, FailurePolicy, UpcastError, feature gate + optional serde_json", "effort": "M", "difficulty": "hard", "requirements": ["R-1","R-2","R-3","R-4","R-5","R-6"] },
    { "phase": 2, "focus": "Schema version metadata: EventData::schema_version default, Event/EventBuilder field threaded through all conversions, EventData derive attribute", "effort": "M", "difficulty": "standard", "requirements": ["R-7","R-8","R-9"] },
    { "phase": 3, "focus": "Failure handling + observability: default Fail policy, explicit DeadLetter sink, counters, structured logs, typed pg error variant", "effort": "S", "difficulty": "standard", "requirements": ["R-10","R-11","R-12","R-13"] },
    { "phase": 4, "focus": "Postgres migration m012 (nullable schema_version INT DEFAULT 1, rename guard, no backfill) + ensure_schema_version_column + registration", "effort": "S", "difficulty": "standard", "requirements": ["R-14","R-15","R-16","R-17"] },
    { "phase": 5, "focus": "Wire epoch_pg read path through the registry; add schema_version to PgDBEvent and all SQL column lists; with_upcasters constructor", "effort": "M", "difficulty": "hard", "requirements": ["R-13","R-22"] },
    { "phase": 6, "focus": "Validation: verify_upcasting_chain contract helper, epoch_mem parity tests, failure-semantics tests, DB-gated epoch_pg end-to-end replay test, fmt/clippy/test gates feature on+off", "effort": "M", "difficulty": "standard", "requirements": ["R-20","R-21","R-22","R-23"] },
    { "phase": 7, "focus": "Design guidance docs (docs/schema-evolution.md) with change-kind matrix and worked examples; CHANGELOG + migration note", "effort": "S", "difficulty": "easy", "requirements": ["R-18","R-19"] }
  ]
}
```
