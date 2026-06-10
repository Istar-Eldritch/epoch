# Delivery Plan: `read_last_event()` + `Command::with_causation_id()` (CLOUD-168)

**Spec:** `0015-cloud168-read-last-event-causation-builder.md` (Status: Approved)
**Plan author:** AI Agent
**Created:** 2026-06-10
**Tracking:** Linear CLOUD-168

---

## Spec Summary

This delivery adds two related capabilities to the `epoch` framework. First, a new
`EventStoreBackend::read_last_event(stream_id)` trait method with a default O(N) impl
(via manual poll loop) and O(1) overrides in both the in-memory and Postgres backends.
Second, a `Command::with_causation_id(uuid)` builder that lets background tasks
reconstruct causation chains without holding a full `Event` reference. Together they
close the gap where a process manager resuming work has no ergonomic way to re-link into
an existing correlation/causation tree.

---

## Critical Blockers

None. All open questions are resolvable during delivery (see Phase 4 for OQ-3/OQ-4
decisions). The `futures-util` dependency question is pre-decided: default to a manual
poll loop; document the fallback in Phase 2 if that proves untenable.

---

## Phase Table

| Phase | Focus | Effort | Difficulty | Parallelism |
|-------|-------|--------|------------|-------------|
| Phase 1 | `Command::with_causation_id` builder + tests | S | standard | sequential — first phase |
| Phase 2 | `EventStoreBackend::read_last_event` default impl + tests | M | hard | sequential — all phases depend on this |
| Phase 3 | `InMemoryEventStore::read_last_event` override + tests | S | standard | parallel with Phase 4 |
| Phase 4 | `PgEventStore::read_last_event` + helper refactor + integration tests | M | hard | parallel with Phase 3 |
| Phase 5 | Example update + Changelog | S | standard | sequential — requires Phases 3 and 4 |
| **Total** | | **3× S + 2× M** | **2 hard phases** | critical path: 1 → 2 → 4 → 5 |

---

## Codebase Grounding (verified before planning)

The following facts were confirmed by reading the source; they drive the decisions below.

| Fact | Location | Implication |
|---|---|---|
| `EventStoreBackend` is `#[async_trait]`; methods use `'life0` lifetime in return types | `epoch_core/src/event_store.rs:23` | New method follows the same `#[async_trait]` signature style |
| `store_events()` already ships a **default impl** | `epoch_core/src/event_store.rs:60` | Precedent for FR-4 default `read_last_event()` |
| `epoch_core` depends on `futures-core` only (no `futures-util`) | `epoch_core/Cargo.toml:15` | Default impl must use manual `poll`/`Stream` or we add `futures-util` (R-2) |
| Existing `Command` builders `caused_by()` / `with_correlation_id()` | `epoch_core/src/aggregate.rs:116,136` | `with_causation_id()` slots in beside them, same `impl` block/bounds |
| `Command.causation_id` / `correlation_id` are `Option<Uuid>` fields | `epoch_core/src/aggregate.rs:47,52` | `with_causation_id` just sets `self.causation_id = Some(..)` |
| PgDBEvent → Event conversion block is **duplicated 3×** (`read_events_since`, `read_events_by_correlation_id`, `trace_causation_chain`) | `epoch_pg/src/event_store.rs:~293, ~430, ~485` | FR-5 helper extraction removes a copy, doesn't add one (R-5) |
| `PgDBEvent` struct + `PgEventStoreError` variants (`DBError`, `DeserializeEventError`, `BuildEventError`, `BUSPublishError`) | `epoch_pg/src/event_store.rs:169, ~228` | Reuse for error mapping (NFR-3) |
| Pg queries already use `self.events_table` via `format!` | `epoch_pg/src/event_store.rs:96,278,…` | FR-5 custom-table support comes for free |
| `read_events_by_correlation_id` uses `fetch_all`; `trace_causation_chain` uses `fetch_optional` | `epoch_pg/src/event_store.rs:423,476` | `fetch_optional` is the established single-row pattern for FR-5 |
| Mem store keeps `stream_events: HashMap<Uuid, Vec<Uuid>>` + `events` map, behind `self.data.lock().await`; events wrapped in `Arc`, cloned via `(**e).clone()` | `epoch_mem/src/event_store.rs:18,244,251` | FR-6 is a direct `.last()` lookup mirroring `read_events_by_correlation_id` |
| Existing command tests live in `epoch_core/tests/command_causation_tests.rs` (plain `#[test]`) | — | Extend, don't create, for Phase 1 |
| Pg integration tests use `#[tokio::test] #[serial]`, `mod common`, `Migrator`, `futures_util::StreamExt` | `epoch_pg/tests/pgeventstore_integration_tests.rs` | Reuse `setup()` harness for Phase 4 tests |
| Example already prints a correlation/causation timeline and calls `trace_causation_chain` | `epoch/examples/event-correlation-causation.rs` | Add a "recovery task" section after the existing flow |
| `CHANGELOG.md` has `[Unreleased] / Added` bullet list | `CHANGELOG.md:8-10` | Append two bullets |

**Key decision (R-2 / §6.2):** `epoch_core` does **not** currently pull in `futures-util`.
To honor CLAUDE.md §5.0 (minimal dependencies), the default `read_last_event()` impl will
be written as a **manual poll loop** using `std::future::poll_fn` + the already-imported
`futures_core::Stream` (`Pin::as_mut().poll_next`), avoiding a new dependency. If the manual
loop proves unreasonably awkward in review, fall back to adding `futures-util` and justify
it in the commit message. **Default: no new dependency.**

---

## Pre-flight

- [ ] `git pull` / confirm clean working tree on feature branch `feat/cloud168-read-last-event`.
- [ ] Baseline: `cargo build --workspace`, `cargo test --workspace` (note Pg tests need a database), `cargo clippy --workspace -- -D warnings`, `cargo fmt --check`. Record current pass/fail so regressions are attributable.
- [ ] Confirm a Postgres instance is reachable for `epoch_pg` integration tests (check `epoch_pg/tests/common`).

---

## Delivery Plan

Each phase is a TDD Red → Green → Refactor cycle. Run `cargo fmt` + targeted `cargo clippy`
after every Green.

---

### Phase 1: `Command::with_causation_id` builder + tests

**Goal:** `Command::with_causation_id(uuid)` is callable and all three causation builder
tests pass, demonstrating that `causation_id` and `correlation_id` are independently
settable with last-write-wins semantics.

**Entry Conditions:**
- Clean working tree on the feature branch (pre-flight complete).
- `cargo test -p epoch_core` passes at baseline.

**Exit Criteria / Verifiable Artifacts:**
- Three new tests in `epoch_core/tests/command_causation_tests.rs` compile and pass:
  `with_causation_id_sets_causation_only`, `with_causation_id_chains_with_correlation_id`,
  `with_causation_id_overwrites_previous_value`.
- `pub fn with_causation_id(mut self, causation_id: Uuid) -> Self` present in
  `epoch_core/src/aggregate.rs` with rustdoc steering users to `caused_by()` when a full
  event is available.
- `cargo clippy -p epoch_core -- -D warnings` and `cargo fmt --check` clean.

**Parallelism:** SEQUENTIAL — this is the first phase; no prior work to depend on.

**Relative Effort:** S — one new method and three tests alongside existing patterns.

**Difficulty:** standard — straightforward builder method; no async or concurrency involved.

**Open Questions / Blockers:** None identified.

#### Implementation detail

**Files:** `epoch_core/tests/command_causation_tests.rs`, `epoch_core/src/aggregate.rs`
**Covers:** FR-7, FR-8, FR-9, NFR-2, NFR-4, SC-5; spec §6.5 tests 1–3.

**Red — add three failing tests beside the existing ones:**
- [ ] `with_causation_id_sets_causation_only` — `causation_id == Some(x)`, `correlation_id == None`.
- [ ] `with_causation_id_chains_with_correlation_id` — chain in **both** orders; assert both fields set independently.
- [ ] `with_causation_id_overwrites_previous_value` — second `with_causation_id` wins; also assert `with_causation_id` after `caused_by` overwrites `causation_id` (last-write-wins, FR-8).

*Verify:* `cargo test -p epoch_core command_causation` fails to compile (method missing) → expected Red.

**Green — add the builder method:**
- [ ] Add `pub fn with_causation_id(mut self, causation_id: Uuid) -> Self` immediately after `with_correlation_id` (same `impl Command<D, C>` block / bounds). Body: `self.causation_id = Some(causation_id); self`. MUST NOT touch `correlation_id`.
- [ ] Add rustdoc per FR-7/FR-9: steer users to `caused_by()` when a full event is available (mitigates R-3), reference `read_last_event()` for the background-task use case.

*Verify:* all three tests pass; `cargo fmt`, `cargo clippy -p epoch_core -- -D warnings`.

**Commit:** `test(core): add failing tests for Command::with_causation_id` →
`feat(command): add Command::with_causation_id builder`

---

### Phase 2: `EventStoreBackend::read_last_event` default impl + tests

**Goal:** A minimal backend double (no `read_last_event` override) compiles and its two
tests pass, proving the default impl drains the stream and returns the last event or
`Ok(None)` without requiring a new crate dependency.

**Entry Conditions:**
- Phase 1 complete; `epoch_core` compiles cleanly on the feature branch.

**Exit Criteria / Verifiable Artifacts:**
- New test file `epoch_core/tests/read_last_event_default_impl_tests.rs` with two passing
  tests: multi-event stream returns final element; empty stream returns `Ok(None)`.
- Minimal backend double in that file compiles **without** defining `read_last_event`
  (SC-4 assertion).
- `async fn read_last_event(&self, stream_id: Uuid) -> Result<Option<Event<Self::EventType>>, Self::Error>`
  present on `EventStoreBackend` with a manual-poll-loop default body and rustdoc
  documenting O(N) complexity and the override expectation.
- No `futures-util` added to `epoch_core/Cargo.toml`.
- All existing `epoch_core` tests still pass (NFR-2 backward compat).
- `cargo clippy -p epoch_core -- -D warnings` and `cargo fmt --check` clean.

**Parallelism:** SEQUENTIAL — requires Phase 1's feature branch setup; all subsequent
phases depend on this trait addition.

**Relative Effort:** M — the manual poll loop over an async trait stream is non-trivial;
the test double requires implementing all trait stubs.

**Difficulty:** hard — manual `poll_fn` + `Pin::as_mut().poll_next` on an async trait
stream without `futures-util`; source-compatibility constraint means every existing backend
must still compile via the default.

**Open Questions / Blockers:**
- If the manual poll loop is rejected in review, fall back to adding `futures-util` to
  `epoch_core/Cargo.toml`, use `StreamExt::next`, and justify the dependency in the commit
  message (R-2). Record the decision explicitly.

#### Implementation detail

**Files:** `epoch_core/tests/read_last_event_default_impl_tests.rs` (new),
`epoch_core/src/event_store.rs`
**Covers:** FR-1, FR-2, FR-3 (contract), FR-4, FR-9, NFR-2, R-1, R-2, SC-4; spec §6.5 tests 4–5.

**Red — new test file with minimal backend double:**
- [ ] Build a **minimal backend double** implementing `EventStoreBackend` using only the previously-required methods (back `read_events`/`read_events_since` with `SliceEventStream`; stub `store_event`, `read_events_by_correlation_id`, `trace_causation_chain`). Crucially: **do not** implement `read_last_event`.
- [ ] Test 4: multi-event stream → default `read_last_event` returns the final element (highest version).
- [ ] Test 5: empty stream → `Ok(None)` (FR-2).

*Verify:* tests fail (method not yet on trait) → Red. Double compiling without `read_last_event` is itself the SC-4 assertion.

**Green — add the trait method with default impl:**
- [ ] Add `async fn read_last_event(&self, stream_id: Uuid) -> Result<Option<Event<Self::EventType>>, Self::Error>` to the `EventStoreBackend` trait, placed after `store_events` / before `read_events_by_correlation_id` (spec §6.2).
- [ ] Default body: consume `self.read_events(stream_id)` and keep the last `Ok` item using a **manual poll loop** (`std::future::poll_fn` + `stream.as_mut().poll_next(cx)`). Propagate `Err` via `?`.
- [ ] Rustdoc per §6.2: define "most recent" = highest `stream_version`; document the default impl is O(N) and instruct backends to override (R-1 mitigation).

*Verify:* both tests pass; whole `epoch_core` still builds; `cargo fmt`, `cargo clippy -p epoch_core -- -D warnings`.

**Commit:** `test(event-store): add default read_last_event tests with minimal backend double` →
`feat(event-store): add EventStoreBackend::read_last_event with default impl`

---

### Phase 3: `InMemoryEventStore::read_last_event` override + tests

**Goal:** The in-memory backend returns the correct last event in O(1) via a direct
`.last()` HashMap lookup, and all three isolation tests pass.

**Entry Conditions:**
- Phase 2 complete; `EventStoreBackend::read_last_event` exists on the trait.

**Exit Criteria / Verifiable Artifacts:**
- Three passing inline tests in `epoch_mem/src/event_store.rs`:
  `read_last_event_returns_latest_event`, `read_last_event_returns_none_for_unknown_stream`,
  `read_last_event_isolated_per_stream`.
- `read_last_event` override present using `data.stream_events.get(&stream_id)
  .and_then(|ids| ids.last()).and_then(|id| data.events.get(id))
  .map(|arc| (**arc).clone())`, wrapped in `Ok(..)`.
- No full stream scan (O(1) via existing `HashMap` indices, NFR-1).
- `cargo test -p epoch_mem` clean; `cargo clippy -p epoch_mem -- -D warnings` and
  `cargo fmt --check` clean.

**Parallelism:** PARALLEL with Phase 4 — both phases only require Phase 2's trait
addition; `epoch_mem` and `epoch_pg` are independent crates.

**Relative Effort:** S — the override is ~5 lines; tests mirror existing mem-store patterns.

**Difficulty:** standard — straightforward HashMap lookup; no async complexity beyond the existing lock pattern.

**Open Questions / Blockers:** None identified.

#### Implementation detail

**Files:** `epoch_mem/src/event_store.rs` (impl + inline `mod tests`)
**Covers:** FR-6, NFR-1, NFR-5, SC-3; spec §6.5 tests 6–8.

**Red — extend inline `mod tests`:**
- [ ] `read_last_event_returns_latest_event` — store 3 events; expect the v3 event with matching `id`/`correlation_id`/`causation_id`.
- [ ] `read_last_event_returns_none_for_unknown_stream` — random/unused `stream_id` → `Ok(None)`.
- [ ] `read_last_event_isolated_per_stream` — two streams; each returns its own last event.

**Green — add the override:**
- [ ] Override `read_last_event` per §6.4: `data.stream_events.get(&stream_id).and_then(|ids| ids.last()).and_then(|id| data.events.get(id)).map(|arc| (**arc).clone())`, wrapped in `Ok(..)`. O(1) lookups + one clone (NFR-1).

*Verify:* `cargo test -p epoch_mem`; `cargo fmt`; `cargo clippy -p epoch_mem -- -D warnings`.

**Commit:** `test(mem): add read_last_event tests` → `feat(mem): implement read_last_event`

---

### Phase 4: `PgEventStore::read_last_event` + helper refactor + integration tests

**Goal:** The Postgres backend returns the correct last event via a single-row indexed
query, the duplicated `PgDBEvent`→`Event` conversion is consolidated into one helper, and
three integration tests pass (latest event with full metadata, empty stream, and
regression on all pre-existing read methods).

**Entry Conditions:**
- Phase 2 complete; `EventStoreBackend::read_last_event` exists on the trait.
- A Postgres instance is reachable (verified in pre-flight).

**Exit Criteria / Verifiable Artifacts:**
- Three new integration tests passing in `epoch_pg/tests/pgeventstore_integration_tests.rs`:
  `test_read_last_event_returns_latest` (asserts `global_sequence`, `correlation_id`,
  `causation_id`, `created_at` on the v3 event), `test_read_last_event_empty_stream_returns_none`,
  and optionally `test_read_last_event_respects_custom_table` (decision logged in commit).
- Private helper `pg_db_event_to_event` extracted; duplicate conversion blocks removed
  from `read_events_since`, `read_events_by_correlation_id`, and `trace_causation_chain`.
- All pre-existing `epoch_pg` integration tests still pass (R-5 regression guard).
- `read_last_event` uses `ORDER BY stream_version DESC LIMIT 1` with `fetch_optional`
  (O(1) via the `UNIQUE (stream_id, stream_version)` index, NFR-1, OQ-1).
- OQ-3 and OQ-4 decisions recorded in commit message or PR description.
- `cargo clippy -p epoch_pg -- -D warnings` and `cargo fmt --check` clean.

**Parallelism:** PARALLEL with Phase 3 — both only depend on Phase 2; independent crates.

**Relative Effort:** M — database query, helper extraction across 3 call sites, integration
test harness with serial async tests, and OQ-3/OQ-4 decisions.

**Difficulty:** hard — involves `sqlx` `fetch_optional`, mechanical refactor across 3
existing call sites (regression risk), serial async integration tests requiring a live
database, and two open questions to resolve in-flight.

**Open Questions / Blockers:**
- **OQ-3:** Include `test_read_last_event_respects_custom_table` only if the existing suite
  has a `with_table` precedent; otherwise rely on `format!(self.events_table)` code review
  and drop the test. Record decision in commit/PR.
- **OQ-4:** Confirm purged last event is returned as-is (metadata survives purge, no
  filtering). Add optional assertion if cheap.

#### Implementation detail

**Files:** `epoch_pg/src/event_store.rs`, `epoch_pg/tests/pgeventstore_integration_tests.rs`
**Covers:** FR-3, FR-5, NFR-1, NFR-3, SC-3; spec §6.5 tests 9–11; R-4, R-5; OQ-1, OQ-3, OQ-4.

**Red — extend integration tests** (reuse `setup()`, `#[tokio::test] #[serial]`, fresh
`Uuid::new_v4()` stream IDs for idempotency/parallel safety, NFR-5):
- [ ] `test_read_last_event_returns_latest` — store v1..v3 with correlation/causation set; assert returned event == stored v3 incl. `global_sequence: Some(_)`, `correlation_id`, `causation_id`, `created_at` (FR-3, R-4).
- [ ] `test_read_last_event_empty_stream_returns_none` — fresh stream ID → `Ok(None)` (FR-2).
- [ ] `test_read_last_event_respects_custom_table` (OQ-3) — include or drop per the decision above.
- [ ] (Optional, OQ-4) assert a purged last event is still returned with metadata intact.

**Refactor — extract conversion helper:**
- [ ] Extract the duplicated `PgDBEvent` → `Event` builder block into a private helper `fn pg_db_event_to_event<D: EventData + DeserializeOwned>(entry: PgDBEvent) -> Result<Event<D>, PgEventStoreError<…>>`.
- [ ] Replace the copies in `read_events_since`, `read_events_by_correlation_id`, and `trace_causation_chain`. Existing integration tests for those methods are the regression guard.

**Green — add the `read_last_event` override:**
- [ ] `format!` query selecting all 12 columns `FROM {events_table} WHERE stream_id = $1 ORDER BY stream_version DESC LIMIT 1`, `fetch_optional`, `let Some(entry) = row else { return Ok(None) };`, then `Ok(Some(pg_db_event_to_event(entry)?))`.

*Verify:* `cargo test -p epoch_pg` (DB required); pre-existing read methods still pass; `cargo fmt`; `cargo clippy -p epoch_pg -- -D warnings`.

**Commit:** `test(pg): add read_last_event integration tests` →
`feat(pg): implement read_last_event with single-row query`
(note helper extraction in commit body)

---

### Phase 5: Example update + Changelog

**Goal:** `cargo run --example event-correlation-causation` runs end-to-end, including a
new "recovery task" section that demonstrates `read_last_event` paired with
`with_causation_id`; `CHANGELOG.md` has two new bullets under `[Unreleased] / Added`.

**Entry Conditions:**
- Phases 3 and 4 complete; both backends have a working `read_last_event`.

**Exit Criteria / Verifiable Artifacts:**
- `cargo run --example event-correlation-causation` exits successfully (SC-6).
- Example output includes the recovery event in the same correlation tree as the original
  events (R-3 pairing pattern demonstrated).
- `CHANGELOG.md` has two new bullets under `[Unreleased] / Added`: one for
  `read_last_event`, one for `with_causation_id` (per spec §6.6).

**Parallelism:** SEQUENTIAL — requires both backends (Phases 3 and 4) to be complete for
the full example flow.

**Relative Effort:** S — additive changes to an existing example file and a changelog.

**Difficulty:** standard — no new logic; demonstrates existing APIs.

**Open Questions / Blockers:** None identified.

#### Implementation detail

**Files:** `epoch/examples/event-correlation-causation.rs`, `CHANGELOG.md`
**Covers:** FR-9, NFR-4, SC-6; spec §6.5 test 12, §6.6.

- [ ] Add a final "recovery task" section after the existing `trace_causation_chain` block: call `event_store.read_last_event(order_id).await?`; if `Some(last_event)`, build a command with `.with_causation_id(last_event.id).with_correlation_id(last_event.correlation_id.unwrap_or(last_event.id))`; dispatch it; reprint/extend the timeline showing the recovery event joins the same correlation tree.
- [ ] Append to `CHANGELOG.md` `[Unreleased] / Added` the two bullets from spec §6.6.

*Verify:* `cargo run --example event-correlation-causation` succeeds (SC-6).

**Commit:** `docs(event-store): demonstrate read_last_event in correlation example; update changelog`

---

Phases 3 and 4 are the only parallelism opportunity. With a single engineer the
recommended sequence is 1 → 2 → 3 → 4 → 5 (or 1 → 2 → 4 → 3 → 5). Critical path
with parallelism: S + M + M + S.

---

## Requirement → Phase Traceability

| Requirement | Phase(s) |
|---|---|
| FR-1 trait method | 2 |
| FR-2 `Ok(None)` for empty/unknown | 2 (default), 3 (mem), 4 (pg) |
| FR-3 highest `stream_version`, full metadata | 2 (contract), 3, 4 |
| FR-4 default impl / source compat | 2 |
| FR-5 Pg single-row override | 4 |
| FR-6 mem override | 3 |
| FR-7 `with_causation_id` builder | 1 |
| FR-8 chaining / last-write-wins | 1 |
| FR-9 rustdoc + example | 1, 2, 5 |
| NFR-1 performance | 3, 4 |
| NFR-2 backward compat | 1, 2 |
| NFR-3 fmt/clippy/no-unwrap | every phase |
| NFR-4 rustdoc + changelog | 1, 2, 5 |
| NFR-5 test hygiene | 1, 2, 3, 4 |

| Success Criterion | Verified by |
|---|---|
| SC-1 workspace tests pass | Final verification gate |
| SC-2 clippy + fmt clean | Final verification gate |
| SC-3 both backends override + tested | Phases 3, 4 |
| SC-4 default impl via minimal double | Phase 2 (double compiles w/o override) |
| SC-5 `with_causation_id` + tests | Phase 1 |
| SC-6 example runs | Phase 5 |
| SC-7 catacloud migration | Out of repo — tracked in CLOUD-168, not delivered here |

---

## Final Verification Gate

Run before declaring done:

- [ ] `cargo fmt --check`
- [ ] `cargo clippy --workspace -- -D warnings`
- [ ] `cargo test --workspace` (with Postgres available for `epoch_pg`)
- [ ] `cargo run --example event-correlation-causation`
- [ ] `lens_diagnostics` / `lsp_diagnostics` clean on all edited files
- [ ] Manual re-read: no `unwrap()`/`expect()` added outside tests; all new public items have rustdoc; `CHANGELOG.md` updated.

---

## Risks & Dependencies

| Risk (spec ref) | Delivery handling |
|---|---|
| R-2 new dep in `epoch_core` | Default to manual poll loop (no `futures-util`); fallback documented in Phase 2 |
| R-3 mismatched correlation when using `with_causation_id` alone | Rustdoc steering (Phase 1) + example pairs it with `with_correlation_id` (Phase 5) |
| R-4 ordering divergence vs catacloud raw SQL | Pg test asserts highest-version event after multiple appends (Phase 4) |
| R-5 helper extraction regressions | Existing `read_events_*`/`trace_*` integration tests are the guard; run them in Phase 4 |
| Postgres availability | Pre-flight checks DB reachability; Phases 1–3 proceed independently if DB is briefly unavailable |
| Out-of-repo SC-7 | Explicitly excluded from this delivery; flagged for CLOUD-168 owner |

---

## Out of Scope (per spec §5)

`read_last_event_metadata()`/`EventMetadata`, catacloud call-site migration, new Pg
migrations, changes to `read_events*`/`store_event*`/tracing APIs, and crate version bumps
beyond the CHANGELOG entry.
