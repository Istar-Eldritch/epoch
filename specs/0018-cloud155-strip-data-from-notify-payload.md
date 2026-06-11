# Spec 0018 — Strip event `data` from `pg_notify` payload (CLOUD-155)

## Status

Proposed.

## Problem

`epoch_notify_event()` currently embeds the full event `data` JSON in every
`pg_notify` payload. PostgreSQL imposes a hard **8 000-byte** limit on
notification payloads; when a payload exceeds it, `pg_notify` raises an error
inside the trigger. Because the trigger fires `AFTER INSERT` within the inserting
transaction, that error rolls back the `INSERT` — **the event is never stored at
all**.

This caused a staging outage on 2026-06-02 when a large billing event hit the
limit. The current trigger definition lives in
`epoch_pg/src/migrations/m007_add_causation_columns.rs:60-86`
(`AddCausationColumns::up`, the `CREATE OR REPLACE FUNCTION epoch_notify_event()`
block) and emits the following keys: `id`, `stream_id`, `stream_version`,
`event_type`, `actor_id`, `purger_id`, `data`, `created_at`, `purged_at`,
`global_sequence`, `causation_id`, `correlation_id`.

### Why the payload body is not needed

In every code path that consumes the notification, the payload is only ever used
as a **wake signal** plus, in one place, an **event identity**; full event data
is always re-read from the database:

1. **`DispatchMode::Async` main listener** —
   `epoch_pg/src/event_bus/mod.rs:847` (`WakeReason::Notification(Ok(_notification))`)
   discards the payload entirely (`_notification`) and re-queries the DB for
   newly committed events. This is the only production dispatch mode.
2. **Temporary catch-up buffer listener** —
   `epoch_pg/src/event_bus/mod.rs:1885-1925` currently deserializes the payload
   into a full `PgDBEvent` (via `serde_json::from_str::<PgDBEvent>` at
   `epoch_pg/src/event_bus/mod.rs:1890`) and reconstructs an
   `Event<Self::EventType>` from it. It only needs to know *which* events
   arrived during catch-up (id + global sequence) so it can re-fetch them from
   the DB; it does not actually need the `data` carried in the payload.
3. **`DispatchMode::Inline`** —
   `epoch_pg/src/event_bus/mod.rs:1786-1794` starts no listener and consumes no
   notification, so it is unaffected.

The catch-up DB queries already `SELECT` the full event columns from the events
table (e.g. the subscriber catch-up query at
`epoch_pg/src/event_bus/mod.rs:1956-1963`), so removing `data` from the payload
loses no information that is not re-fetched from the source of truth.

---

## Goals

- Remove `data` (and the other large/non-essential columns) from the
  `pg_notify` payload so that arbitrarily large events can be stored without
  hitting the 8 KB limit.
- Keep `DispatchMode::Async` real-time delivery and catch-up gap-freedom
  behaviour identical.
- No behavioural change for `DispatchMode::Inline`.

## Non-Goals

- Changing the events table schema or any stored column.
- Changing delivery semantics (still at-least-once).
- Changing the catch-up DB query that selects full event columns.

---

## Solution Overview

Three changes, all in `epoch_pg`:

1. New migration `m010` that `CREATE OR REPLACE`s `epoch_notify_event()` to emit
   only identity/sequencing metadata.
2. A private `NotifyPayload` deserialization struct in `event_bus/mod.rs`.
3. Re-type the catch-up buffer channel from `Event<Self::EventType>` to a
   lightweight `(Uuid, u64)` identity tuple, and change the buffer drain to
   re-fetch full event rows from the DB by sequence range.

---

## Detailed Design

### 1. New migration `m010_strip_data_from_notify_payload`

**New file:** `epoch_pg/src/migrations/m010_strip_data_from_notify_payload.rs`

The next available version is **10** (the current latest is `CreateGapTimeoutLog`
= version 9, defined in `epoch_pg/src/migrations/m009_create_gap_timeout_log.rs`
and registered in `epoch_pg/src/migrations/mod.rs:42` / `mod.rs:74`).

Implement the `Migration` trait (`epoch_pg/src/migrations/mod.rs:147-187`)
following the existing pattern of `AddCausationColumns`
(`epoch_pg/src/migrations/m007_add_causation_columns.rs`):

```rust
//! Migration 010: Strip `data` from the `epoch_notify_event()` NOTIFY payload.
//!
//! PostgreSQL truncates/erres on `pg_notify` payloads larger than 8 000 bytes.
//! Because the NOTIFY trigger fires inside the inserting transaction, an
//! oversized payload error rolls back the INSERT and the event is lost
//! (see CLOUD-155).
//!
//! In `DispatchMode::Async` the notification is only a wake signal: the listener
//! re-queries the database for committed events. The catch-up buffer listener
//! likewise only needs event identity (id + global_sequence) and fetches full
//! data from the database. This migration therefore replaces
//! `epoch_notify_event()` so the payload carries identity/sequencing metadata
//! only — never `data`, `purger_id`, `purged_at`, `causation_id`, or
//! `correlation_id`.

use async_trait::async_trait;
use sqlx::{Postgres, Transaction};

use super::{Migration, MigrationError};

/// Replaces `epoch_notify_event()` to drop `data` from the NOTIFY payload.
pub struct StripDataFromNotifyPayload;

#[async_trait]
impl Migration for StripDataFromNotifyPayload {
    fn version(&self) -> i64 {
        10
    }

    fn name(&self) -> &'static str {
        "strip_data_from_notify_payload"
    }

    async fn up<'a>(&self, tx: &mut Transaction<'a, Postgres>) -> Result<(), MigrationError> {
        sqlx::query(
            r#"
            CREATE OR REPLACE FUNCTION epoch_notify_event()
            RETURNS TRIGGER AS $$
            BEGIN
                PERFORM pg_notify(
                    TG_ARGV[0],
                    json_build_object(
                        'id',              NEW.id,
                        'stream_id',       NEW.stream_id,
                        'stream_version',  NEW.stream_version,
                        'event_type',      NEW.event_type,
                        'actor_id',        NEW.actor_id,
                        'global_sequence', NEW.global_sequence,
                        'created_at',      NEW.created_at
                    )::text
                );
                RETURN NEW;
            END;
            $$ LANGUAGE plpgsql;
            "#,
        )
        .execute(&mut **tx)
        .await?;

        Ok(())
    }
}
```

Notes:

- `CREATE OR REPLACE FUNCTION` keeps the existing trigger binding intact; only
  the function body changes. The trigger itself (created in an earlier
  migration) is untouched.
- The payload retains `global_sequence` (used by the buffer drain) and `id`
  (used for dedup/identity). The remaining keys (`stream_id`, `stream_version`,
  `event_type`, `actor_id`, `created_at`) are small, bounded, and kept for
  observability/debugging of NOTIFY traffic; none of them carry user-controlled
  unbounded data.
- The migration is forward-only (consistent with the “No Rollback Support” note
  at `epoch_pg/src/migrations/mod.rs:52-72`). Applying it on a DB that already
  has the m007 function definition simply replaces it.

### 1a. Register `m010` in the migration registry

**File:** `epoch_pg/src/migrations/mod.rs`

- Add the module declaration alongside the others
  (`epoch_pg/src/migrations/mod.rs:33-42`):
  ```rust
  mod m010_strip_data_from_notify_payload;
  ```
- Add the `use` import alongside the others
  (`epoch_pg/src/migrations/mod.rs:44-53`):
  ```rust
  use m010_strip_data_from_notify_payload::StripDataFromNotifyPayload;
  ```
- Append to the `MIGRATIONS` array (`epoch_pg/src/migrations/mod.rs:74-84`), as
  the new last entry:
  ```rust
  &CreateGapTimeoutLog,
  &StripDataFromNotifyPayload,
  ```

The existing registry tests (`migrations_are_in_order`,
`all_migrations_have_unique_versions`, `all_migrations_have_unique_names` at
`epoch_pg/src/migrations/mod.rs:354-401`) will validate ordering/uniqueness of
the new entry with no changes needed.

### 2. Add a private `NotifyPayload` struct

**File:** `epoch_pg/src/event_bus/mod.rs`

Add near the top-level type definitions in the module (e.g. alongside the other
internal structs around `epoch_pg/src/event_bus/mod.rs:38-67`). `Uuid` is
already imported at `epoch_pg/src/event_bus/mod.rs:33` and `serde::de` is
imported at line 25, but the struct uses the derive directly:

```rust
/// Minimal notification payload — identity/wake signal only.
///
/// Mirrors the keys emitted by `epoch_notify_event()` after migration m010.
/// Full event data is always fetched from the database, so only the fields
/// needed to identify which events arrived during catch-up are deserialized.
#[derive(Debug, serde::Deserialize)]
struct NotifyPayload {
    id: Uuid,
    global_sequence: Option<i64>,
}
```

`global_sequence` is `Option<i64>` to match the DB column type
(`PgDBEvent.global_sequence: Option<i64>` at `epoch_pg/src/event_store.rs:189-192`)
and tolerate rows where it is absent.

### 3. Re-type the catch-up buffer channel

**File:** `epoch_pg/src/event_bus/mod.rs`

Currently the buffer channel carries fully-reconstructed `Event<Self::EventType>`
values (`epoch_pg/src/event_bus/mod.rs:1841-1843`):

```rust
let buffer_size = config.catch_up_buffer_size;
let (buffer_tx, mut buffer_rx) =
    tokio::sync::mpsc::channel::<Event<Self::EventType>>(buffer_size);
```

Change the channel item type to a `(Uuid, u64)` identity tuple `(event id,
global sequence)`:

```rust
let buffer_size = config.catch_up_buffer_size;
let (buffer_tx, mut buffer_rx) =
    tokio::sync::mpsc::channel::<(Uuid, u64)>(buffer_size);
```

### 4. Simplify the buffer listener

**File:** `epoch_pg/src/event_bus/mod.rs:1885-1916` (the
`Ok(Ok(notification)) => { ... }` arm inside the spawned buffer task)

Replace the `PgDBEvent` deserialization + full `Event` reconstruction with a
`NotifyPayload` deserialization that forwards only identity:

```rust
Ok(Ok(notification)) => {
    if let Ok(payload) =
        serde_json::from_str::<NotifyPayload>(notification.payload())
    {
        let global_seq = payload.global_sequence.unwrap_or(0) as u64;
        // Send to bounded channel - will apply backpressure if full
        if buffer_tx.send((payload.id, global_seq)).await.is_err() {
            // Receiver dropped, catch-up is complete
            break;
        }
        log::debug!("Buffered event during catch-up: {:?}", payload.id);
    }
}
```

The surrounding `loop { match tokio::time::timeout(...) }` structure and the
`Ok(Err(_))` / `Err(_)` arms (`epoch_pg/src/event_bus/mod.rs:1918-1929`) are
unchanged.

### 5. Buffer drain: collect max sequence, then fetch from DB

**File:** `epoch_pg/src/event_bus/mod.rs:2129-2236` (from `buffer_handle.abort();`
through the end of the buffer-processing block and its final checkpoint flush and
log).

The current drain (`epoch_pg/src/event_bus/mod.rs:2132-2141`) closes the receiver
and collects full `Event` values into `buffered_events`, then iterates them
(`epoch_pg/src/event_bus/mod.rs:2142-2204`) applying dedup (`seq <=
current_sequence`), retry/DLQ via `process_event_with_retry`
(`epoch_pg/src/event_bus/retry.rs`, re-exported at
`epoch_pg/src/event_bus/mod.rs:14`), and checkpoint flushing via
`flush_checkpoint` / `should_flush_checkpoint` / `PendingCheckpoint`.

Replace it so that the channel now only yields `(Uuid, u64)` identities. Drain to
find the highest sequence seen during catch-up, then issue a single bounded query
for the events that arrived in the `(current_sequence, max_buffered_seq]` window
and run them through the existing process/checkpoint logic:

```rust
// Stop the buffer listener
buffer_handle.abort();

// Drain the buffer to find the highest sequence seen during catch-up.
// Close the receiver to signal the sender to stop.
buffer_rx.close();
let mut max_buffered_seq = current_sequence;
let mut buffered_count = 0usize;
while let Some((_id, global_seq)) = buffer_rx.recv().await {
    buffered_count += 1;
    if global_seq > max_buffered_seq {
        max_buffered_seq = global_seq;
    }
}

let mut processed_from_buffer = 0u64;

// Only query if at least one buffered event is newer than our checkpoint.
// The `global_sequence > current_sequence` predicate also performs the
// dedup that the old in-memory loop did explicitly.
if max_buffered_seq > current_sequence {
    let rows: Vec<PgDBEvent> = sqlx::query_as(&format!(
        "SELECT id, stream_id, stream_version, event_type, data, \
         created_at, actor_id, purger_id, purged_at, \
         global_sequence, causation_id, correlation_id \
         FROM {} WHERE global_sequence > $1 AND global_sequence <= $2 \
         ORDER BY global_sequence ASC",
        config.events_table,
    ))
    .bind(current_sequence as i64)
    .bind(max_buffered_seq as i64)
    .fetch_all(&pool)
    .await?;

    for row in rows {
        let event_global_seq = row.global_sequence.unwrap_or(0) as u64;
        let event_id = row.id;

        // Deserialize, mirroring the catch-up loop's skip-on-error behaviour
        // (epoch_pg/src/event_bus/mod.rs:1989-2046): undeserializable variants
        // advance the checkpoint rather than looping forever.
        let data: Option<Self::EventType> =
            match row.data.map(|d| serde_json::from_value(d)).transpose() {
                Ok(d) => d,
                Err(e) => {
                    warn!(
                        "Buffer processing: skipping event {} (type: '{}', \
                         global_seq: {}) for '{}': failed to deserialize: {}. \
                         Advancing checkpoint past this event.",
                        event_id, row.event_type, event_global_seq, subscriber_id, e
                    );
                    current_sequence = event_global_seq;
                    // ...track + maybe-flush pending_checkpoint (same as below)...
                    continue;
                }
            };

        let event = Arc::new(Event {
            id: row.id,
            stream_id: row.stream_id,
            stream_version: row.stream_version as u64,
            event_type: row.event_type,
            actor_id: row.actor_id,
            purger_id: row.purger_id,
            data,
            created_at: row.created_at,
            purged_at: row.purged_at,
            global_sequence: Some(event_global_seq),
            causation_id: row.causation_id,
            correlation_id: row.correlation_id,
        });

        let result =
            process_event_with_retry(&observer, &event, &subscriber_id, &config, &pool)
                .await;

        // Track + maybe-flush pending_checkpoint — identical to the existing
        // buffer loop (epoch_pg/src/event_bus/mod.rs:2163-2192).
        match &mut pending_checkpoint {
            Some(pending) => pending.update(event_global_seq, event_id),
            None => {
                pending_checkpoint =
                    Some(PendingCheckpoint::new(event_global_seq, event_id))
            }
        }
        if let Some(pending) = pending_checkpoint
            .take_if(|p| should_flush_checkpoint(p, &config.checkpoint_mode))
            && let Err(e) = flush_checkpoint(
                &pool,
                &config.events_table,
                &subscriber_id,
                &pending,
                &mut checkpoint_cache,
            )
            .await
        {
            error!(
                "Buffer processing: failed to flush checkpoint for '{}': {}",
                subscriber_id, e
            );
            pending_checkpoint = Some(pending);
        }

        current_sequence = event_global_seq;
        processed_from_buffer += 1;

        if let ProcessResult::Success = result {
            log::debug!(
                "Processed buffered event {} for '{}'",
                event_id, subscriber_id
            );
        }
    }
}
```

The final “flush remaining pending checkpoint” block
(`epoch_pg/src/event_bus/mod.rs:2207-2222`) is retained as-is.

The closing log line (`epoch_pg/src/event_bus/mod.rs:2224-2235`) currently reports
`buffered_count as u64 - processed_from_buffer` as the “deduplicated” count.
With the new design, `buffered_count` is the number of *notifications drained*
(wake signals) rather than reconstructed events, and `processed_from_buffer` is
the number of DB rows processed. These two counters are no longer directly
subtractable as a “deduplicated” figure. The log statement must be adjusted to
avoid a misleading metric, e.g.:

```rust
if buffered_count > 0 {
    info!(
        "Drained {} buffered notifications for '{}', processed {} events up to \
         sequence {}, checkpoint now at {}",
        buffered_count, subscriber_id, processed_from_buffer,
        max_buffered_seq, current_sequence
    );
}
```

(The exact wording is non-normative; the requirement is that the message uses
the available counters truthfully and does not compute a bogus dedup delta.)

---

## Correctness Notes

- **Gap-freedom preserved.** The catch-up loop processes everything up to
  `current_sequence`. The buffer captures the identities of all events that the
  temporary listener saw during catch-up. After catch-up, any event with
  `global_sequence` in `(current_sequence, max_buffered_seq]` is fetched and
  processed in order. Events `<= current_sequence` are excluded by the SQL
  `WHERE` clause, which subsumes the old in-memory dedup check
  (`epoch_pg/src/event_bus/mod.rs:2145-2154`).
- **No new race.** The main listener is still started after this block and
  re-checks checkpoints per event, so any event with sequence beyond
  `max_buffered_seq` (or any missed by the buffer listener) is still delivered by
  the main listener’s DB re-query — the same safety argument documented at
  `epoch_pg/src/event_bus/mod.rs:2235-2245`.
- **Bounded query.** The drain query is bounded by `max_buffered_seq` and does
  not use `LIMIT`; the window equals the number of events committed during the
  catch-up interval, which is the same volume that previously sat in the buffer.
- **`?` error propagation.** The new `fetch_all(...).await?` matches the existing
  fallible style of the surrounding `subscribe` future (e.g. the catch-up query
  at `epoch_pg/src/event_bus/mod.rs:1966-1969`).

---

## Files Changed

| File | Change |
|------|--------|
| `epoch_pg/src/migrations/m010_strip_data_from_notify_payload.rs` | **New** — migration `StripDataFromNotifyPayload` (version 10) that `CREATE OR REPLACE`s `epoch_notify_event()` without `data` |
| `epoch_pg/src/migrations/mod.rs` | Declare `mod m010_…`, `use StripDataFromNotifyPayload`, append to `MIGRATIONS` |
| `epoch_pg/src/event_bus/mod.rs` | Add `NotifyPayload`; change buffer channel type (`:1842`), buffer listener arm (`:1885-1916`), and buffer drain (`:2129-2235`) |

No changes to `epoch_pg/src/event_store.rs` (`PgDBEvent` at
`epoch_pg/src/event_store.rs:163-196` and its columns are reused unchanged) or
any other file.

---

## What Does NOT Change

- `DispatchMode::Async` main listener loop — already ignores the payload body
  (`WakeReason::Notification(Ok(_notification))` at
  `epoch_pg/src/event_bus/mod.rs:847`).
- `DispatchMode::Inline` — no listener path (`epoch_pg/src/event_bus/mod.rs:1786-1794`).
- The events table schema and all stored columns.
- The subscriber catch-up DB query
  (`epoch_pg/src/event_bus/mod.rs:1956-1963`) — still selects full event columns.
- `process_event_with_retry`, `flush_checkpoint`, `should_flush_checkpoint`,
  and `PendingCheckpoint` behaviour.

---

## Phased Delivery Plan

Work is split into three phases. **Phase 1** (migration) and **Phase 2**
(event-bus refactor) touch disjoint files and may proceed in parallel; **Phase 3**
(verification) depends on both. Each phase follows TDD (red → green → refactor).

### Phase 1 — Migration `m010` + registry wiring

- **Goal:** Replace `epoch_notify_event()` via a new forward-only migration so
  the `pg_notify` payload carries identity/sequencing metadata only, never
  `data`. This alone fixes the 8 KB rollback for the production `Async` path,
  because the main listener already ignores the payload body.
- **Scope — files to create:**
  - `epoch_pg/src/migrations/m010_strip_data_from_notify_payload.rs` — new
    `StripDataFromNotifyPayload` struct implementing `Migration`
    (`version() -> 10`, `name() -> "strip_data_from_notify_payload"`, `up()`
    issuing the `CREATE OR REPLACE FUNCTION epoch_notify_event()` from
    §"Detailed Design 1"). Mirror the pattern of `AddCausationColumns`
    (`epoch_pg/src/migrations/m007_add_causation_columns.rs:60-86`).
- **Scope — files to modify:**
  - `epoch_pg/src/migrations/mod.rs:38` — add `mod m010_strip_data_from_notify_payload;`
    after `mod m009_create_gap_timeout_log;`.
  - `epoch_pg/src/migrations/mod.rs:48` — add
    `use m010_strip_data_from_notify_payload::StripDataFromNotifyPayload;` after
    the `m009` import.
  - `epoch_pg/src/migrations/mod.rs:79` — append `&StripDataFromNotifyPayload,`
    as the new last entry of the `MIGRATIONS` array, after `&CreateGapTimeoutLog,`.
- **Out of bounds:** No changes to `event_bus/mod.rs`, `event_store.rs`, the
  events table schema, or any earlier migration. Do not add a `down()` (system
  is forward-only, `epoch_pg/src/migrations/mod.rs:52-72`).
- **Entry conditions:** Spec approved. `cargo build -p epoch_pg` green on `main`.
- **Exit criteria:**
  - `cargo test -p epoch_pg migrations::tests` passes (`migrations_are_in_order`,
    `all_migrations_have_unique_versions`, `all_migrations_have_unique_names` at
    `epoch_pg/src/migrations/mod.rs:354-401` validate the version-10 entry with
    no edits to those tests).
  - `cargo build -p epoch_pg` compiles with the new module registered.
- **Parallelism:** Independent of Phase 2 (disjoint files). Can run concurrently.
- **Effort:** ~0.5 day (S).
- **Difficulty:** standard.
- **Blockers:** None.

### Phase 2 — Event-bus catch-up payload refactor

- **Goal:** Make the temporary catch-up buffer listener tolerate the slimmed
  payload by carrying only `(Uuid, u64)` event identity through the channel and
  re-fetching full event rows from the DB on drain — preserving gap-freedom,
  dedup, retry/DLQ, and checkpoint semantics.
- **Scope — files to modify (`epoch_pg/src/event_bus/mod.rs`):**
  - Add private `struct NotifyPayload { id: Uuid, global_sequence: Option<i64> }`
    (`#[derive(Debug, serde::Deserialize)]`) near the internal type definitions
    (~`epoch_pg/src/event_bus/mod.rs:38-67`). `Uuid` already imported at line 33.
  - Re-type the buffer channel at `epoch_pg/src/event_bus/mod.rs:1841-1843`
    from `mpsc::channel::<Event<Self::EventType>>(buffer_size)` to
    `mpsc::channel::<(Uuid, u64)>(buffer_size)`.
  - Rewrite the listener arm `Ok(Ok(notification)) => { ... }` at
    `epoch_pg/src/event_bus/mod.rs:1885-1916` to deserialize `NotifyPayload`
    and `buffer_tx.send((payload.id, global_seq))`; leave the `Ok(Err(_))` /
    `Err(_)` arms (`:1918-1929`) untouched.
  - Replace the buffer drain block at `epoch_pg/src/event_bus/mod.rs:2129-2235`:
    drain `buffer_rx` to compute `max_buffered_seq`, then a single bounded
    `sqlx::query_as::<_, PgDBEvent>` over
    `global_sequence > current_sequence AND <= max_buffered_seq` feeding the
    existing `process_event_with_retry` / `PendingCheckpoint` /
    `should_flush_checkpoint` / `flush_checkpoint` logic; fix the closing log
    line (`:2224-2235`) to stop computing a bogus dedup delta.
- **Scope — tests to add:** A `#[test]` in the event-bus test module
  (`epoch_pg/src/event_bus/mod.rs:2253`) asserting
  `serde_json::from_str::<NotifyPayload>` parses the exact JSON the new trigger
  emits (keys `id`, `stream_id`, `stream_version`, `event_type`, `actor_id`,
  `global_sequence`, `created_at`), ignores absent `data`, and yields the
  expected `id`/`global_sequence`.
- **Out of bounds:** Do not touch the `WakeReason::Notification` main-listener
  arm (`:847`), the `Inline` path (`:1786-1794`), the catch-up DB query
  (`:1956-1963`), `PgDBEvent` (`epoch_pg/src/event_store.rs:163-196`), or
  `retry.rs`. Keep the final "flush remaining pending checkpoint" block
  (`:2207-2222`) and the post-block safety comment (`:2235-2245`) as-is.
- **Entry conditions:** Spec approved. Phase 1 not required to start, but the
  branch must compile against the trigger's intended payload shape (the
  `NotifyPayload` test asserts that shape).
- **Exit criteria:**
  - The `NotifyPayload` unit test passes.
  - `cargo build -p epoch_pg` compiles with the re-typed channel, listener, and
    drain (the channel re-type, listener, and drain must land atomically to
    type-check).
  - No behavioural change to dedup/checkpoint/skip-on-deserialize logic
    (verified by inspection against §"Correctness Notes").
- **Parallelism:** Independent of Phase 1 (disjoint files); the three edits
  within this phase are coupled and must land in one atomic change.
- **Effort:** ~1.5 days (S).
- **Difficulty:** hard (channel re-type ripples through listener + drain; must
  preserve gap-freedom and checkpoint invariants).
- **Blockers:** None.

### Phase 3 — Integration verification, lint, and acceptance

- **Goal:** Prove no regression end-to-end and satisfy CLOUD-155 acceptance.
- **Scope — files to modify:** None expected (verification only). If an existing
  catch-up/buffering integration test under `epoch_pg/tests` needs an assertion
  tightened to cover large-payload storage, add it here; do not modify product
  code unless a defect is found.
- **Out of bounds:** No product-code changes beyond fixes for defects surfaced
  by the test run; no changes to delivery semantics.
- **Entry conditions:** Phases 1 and 2 merged/branch-integrated and compiling.
- **Exit criteria (acceptance):**
  - A large-event store (> 8 KB `data`) succeeds with no truncation/rollback.
  - `DispatchMode::Async` listener still wakes on new events.
  - Catch-up buffer captures identities; full data fetched for
    `(current_sequence, max_buffered_seq]`.
  - `cargo clippy -p epoch_pg -- -D warnings` passes with zero warnings.
  - `cargo test -p epoch_pg` passes with zero failures.
- **Parallelism:** Sequential — depends on both Phase 1 and Phase 2.
- **Effort:** ~0.5 day (S).
- **Difficulty:** standard.
- **Blockers:** Requires a running PostgreSQL instance for integration tests;
  depends on completion of Phases 1 and 2.

---

## Acceptance Criteria (matches CLOUD-155)

1. `epoch_notify_event()` no longer includes `data` in the `pg_notify` payload.
2. Events with arbitrarily large payloads store successfully (no 8 KB
   truncation/rollback error).
3. `DispatchMode::Async` listener still wakes correctly on new events.
4. The catch-up buffer still captures event identity correctly; full data is
   fetched from the DB for the `(current_sequence, max_buffered_seq]` window.
5. `cargo clippy -p epoch_pg -- -D warnings` passes with zero warnings.
6. `cargo test -p epoch_pg` passes with zero failures.

---

## Phases (JSON)

```json
{
  "phases": [
    {
      "phase": 1,
      "focus": "Strip-data notify migration m010",
      "effort": "S",
      "difficulty": "standard"
    },
    {
      "phase": 2,
      "focus": "Event-bus catch-up payload refactor",
      "effort": "S",
      "difficulty": "hard"
    },
    {
      "phase": 3,
      "focus": "Integration verification and acceptance",
      "effort": "S",
      "difficulty": "standard"
    }
  ]
}
```
