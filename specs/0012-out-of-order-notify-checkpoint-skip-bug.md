# Fix: Out-of-Order NOTIFY Delivery Causes Events to be Silently Skipped

**Spec ID:** 0012  
**Status:** 📋 Draft  
**Created:** 2026-02-17  
**Severity:** High — events silently dropped, no DLQ entry, no error log  

---

## Problem Statement

The `PgEventBus` real-time listener uses an in-memory checkpoint cache to deduplicate events. It skips any event whose `global_sequence <= last_seen_sequence` for that subscriber. However, PostgreSQL's `nextval()` is non-transactional: two concurrent transactions can obtain sequences N and N+1 but commit in reverse order. This causes NOTIFY messages to arrive out of global-sequence order, making the deduplication logic incorrectly discard legitimate events.

**Result:** Silent data loss — the event is never processed, no error is raised, no DLQ entry is created.

### Root Cause

1. **Transaction A** obtains `global_sequence = N`
2. **Transaction B** obtains `global_sequence = N+1`
3. **Transaction B** commits first → NOTIFY for N+1 delivered
4. Listener processes N+1, sets `checkpoint_cache[subscriber] = N+1`
5. **Transaction A** commits → NOTIFY for N delivered
6. Listener checks: `N <= N+1` → **skips event N**

### Observed in Production

In Catacloud's seed process, 5 subscription invoices were processed concurrently. One `OrganizationSubscriptionActivated` event (seq 7898) was delivered out-of-order after seq 7899 had already been processed. The `SubscriptionRenewalSaga` never received the event, no `CreditsGranted` was emitted, and the process timed out waiting for 17 credit grants (only 16 materialized).

### Scope

| Property | Value |
|---|---|
| Affected | `PgEventBus` real-time listener path (NOTIFY-based delivery) |
| Unaffected | Catch-up path (queries DB with `ORDER BY global_sequence ASC` after transactions commit) |
| Visibility | None — no error, no warning, no DLQ entry |
| Reproducibility | Intermittent; requires concurrent transactions with adjacent sequences committing out of order |

---

## Solution Overview

Replace the current "process event from NOTIFY payload" approach with a **DB-query-driven model** where NOTIFY serves only as a **wake-up signal**. Combined with **gap-aware contiguous checkpoint tracking**, this ensures no events are skipped regardless of NOTIFY delivery order.

### Key Insight

The current listener deserializes the event directly from the NOTIFY payload and processes that single event. This couples correctness to NOTIFY delivery order. Instead:

1. **NOTIFY = "something new happened"** — triggers a DB query, payload is ignored for processing purposes
2. **DB query returns committed events in sequence order** — inherently correct ordering
3. **Contiguous checkpoint tracking** — only advances the persisted checkpoint when there are no gaps, handling the case where earlier-sequenced transactions haven't committed yet
4. **Processed-ahead set** — tracks events processed above the contiguous checkpoint to avoid reprocessing
5. **Gap timeout** — handles rolled-back transactions (permanent gaps) by advancing past gaps that haven't filled within a configurable timeout

---

## Key Design Decisions

### Why NOTIFY as Wake-Up Signal (Option D from original analysis)

| Option | Complexity | Correctness | Performance |
|--------|-----------|-------------|-------------|
| A: Gap-aware re-fetch | Medium | Correct if gaps detected | Extra DB query on out-of-order only |
| B: Sliding window | High | Window size is a tuning parameter | Low overhead |
| C: Always gap-fill range | Low | **Same bug** — DB query may miss uncommitted events | DB query per NOTIFY |
| **D: NOTIFY as wake-up + gap tracking** | **Medium** | **Correct** | **One indexed DB query per NOTIFY** |

Options A and C fail to account for the fact that at query time, earlier-sequenced events may not yet be visible (uncommitted). Option D combined with contiguous checkpoint tracking handles this correctly.

### Why Not Pure Option D

A naive implementation of Option D (query DB, process all, advance checkpoint) has the same bug: if event N+1 is visible in the DB but N isn't (N's transaction hasn't committed yet), processing N+1 and advancing the checkpoint to N+1 would skip N when it later commits.

The fix is **contiguous checkpoint tracking**: the persisted checkpoint only advances to the highest sequence where all prior events have been seen. Events beyond a gap are processed (added to a `processed_ahead` set) but don't advance the checkpoint until the gap fills.

### Gap Timeout for Rolled-Back Transactions

PostgreSQL sequences skip numbers for rolled-back transactions — those sequence numbers will never have corresponding events. Without a timeout, the checkpoint would be stuck forever at the gap.

Default timeout: **5 seconds**. This is conservative — most transactions complete in milliseconds. The timeout can be tuned via configuration.

When a gap times out, we verify it's truly missing by checking the DB query results: if events above the gap are visible but the gap sequence is not, and sufficient time has passed, we advance past it.

### Reuse of `read_all_events_since`

The existing `read_all_events_since` method already implements the exact DB query needed. Rather than duplicating query logic, the notification handler will use a similar query pattern internally (operating directly on `PgPool` within the spawned task).

### Performance Impact

- **Normal case**: NOTIFY arrives → one indexed DB query → typically returns 1 event → same latency as before
- **Burst case**: Multiple NOTIFYs arrive rapidly → first query returns several events, subsequent queries find nothing new
- **Out-of-order case**: Events processed with slight checkpoint lag until gap fills (sub-second)
- **Network overhead**: One additional DB round-trip per NOTIFY vs. zero before. The query `WHERE global_sequence > $1 ORDER BY global_sequence ASC LIMIT $2` uses the index on `global_sequence` and typically returns 0–2 rows.

---

## Data Structures

### New: `SubscriberState`

Replaces the simple `u64` in `checkpoint_cache`:

```rust
/// Tracks per-subscriber processing state for gap-aware checkpoint advancement.
struct SubscriberState {
    /// The highest contiguous global_sequence that has been processed.
    /// All events with sequence <= this value have been processed or confirmed missing.
    /// This is the value persisted to the database checkpoint.
    contiguous_checkpoint: u64,

    /// Global sequences that have been processed but are above contiguous_checkpoint.
    /// These are events processed "ahead" of a gap. Bounded by the number of
    /// concurrent uncommitted transactions (typically 0-2 entries).
    processed_ahead: HashSet<u64>,

    /// Tracks when gaps were first observed, for timeout-based resolution.
    /// Key: the missing global_sequence. Value: when we first noticed it missing.
    gap_first_seen: HashMap<u64, Instant>,
}
```

**Memory characteristics:**
- `processed_ahead`: Empty during normal (in-order) operation. Contains 1–2 entries during brief out-of-order windows. Bounded by number of concurrent transactions.
- `gap_first_seen`: Same size as number of active gaps. Entries are removed when gaps fill or time out.

### Modified: Replace `checkpoint_cache: HashMap<String, u64>`

Replace with:
```rust
subscriber_states: HashMap<String, SubscriberState>
```

The `contiguous_checkpoint` field serves the same role as the old `u64` value for checkpoint persistence and deduplication.

---

## Algorithm

### Notification Handler (replaces current event processing in `start_listener`)

```
On NOTIFY received:
    log::debug!("NOTIFY received, querying DB for pending events")
    
    for each subscriber in projections:
        state = subscriber_states.get_or_initialize(subscriber_id)
        
        loop:  // Process all available events in batches
            events = query DB:
                SELECT ... FROM epoch_events
                WHERE global_sequence > state.contiguous_checkpoint
                ORDER BY global_sequence ASC
                LIMIT catch_up_batch_size
            
            if events.is_empty():
                break
            
            processed_any = false
            for event in events:
                seq = event.global_sequence
                
                if seq in state.processed_ahead:
                    continue  // Already processed out-of-order
                
                // Deserialize event data (same error handling as current code)
                match deserialize(event.data):
                    Err(e):
                        // Undeserializable event — treat as processed, advance past it
                        warn!("Skipping undeserializable event ...")
                        state.processed_ahead.insert(seq)
                        processed_any = true
                        continue
                    Ok(data):
                        // Process with retry/DLQ (same as current code)
                        process_event_with_retry(...)
                        state.processed_ahead.insert(seq)
                        processed_any = true
            
            // Advance contiguous checkpoint
            advance_contiguous_checkpoint(state, &events)
            
            // Update pending_checkpoints for batched mode (using contiguous_checkpoint)
            update_pending_checkpoint(state.contiguous_checkpoint, ...)
            
            // If we got fewer events than batch_size, no more to process
            if events.len() < catch_up_batch_size || !processed_any:
                break
```

### `advance_contiguous_checkpoint` 

```
fn advance_contiguous_checkpoint(state: &mut SubscriberState, db_events: &[Event]):
    let visible_seqs: BTreeSet = db_events.map(|e| e.global_sequence).collect()
    
    loop:
        let next = state.contiguous_checkpoint + 1
        
        if next in state.processed_ahead:
            // This sequence was processed (or skipped due to deser failure)
            state.processed_ahead.remove(next)
            state.gap_first_seen.remove(next)
            state.contiguous_checkpoint = next
            continue
        
        if !visible_seqs.is_empty() && next < visible_seqs.first():
            // `next` is not in DB results and not in processed_ahead.
            // This is a gap — either uncommitted transaction or rollback.
            
            if let Some(first_seen) = state.gap_first_seen.get(next):
                if first_seen.elapsed() > gap_timeout:
                    // Gap has been observed long enough — assume rolled back
                    log::info!("Advancing past gap at seq {} (timed out after {:?})", next, gap_timeout)
                    state.gap_first_seen.remove(next)
                    state.contiguous_checkpoint = next
                    continue
                // else: gap not old enough, wait for it
            else:
                // First time seeing this gap
                state.gap_first_seen.insert(next, Instant::now())
            
            break  // Can't advance further
        
        // No more events to consider
        break
```

### Periodic Timer (existing `flush_interval` tick)

The existing `tokio::select!` already has a periodic timer for flushing batched checkpoints. On each tick, in addition to flushing expired checkpoints, also re-run the event processing loop for all subscribers. This handles:
- Filling gaps after delayed commits
- Advancing past timed-out gaps (rolled-back transactions)
- Catching any missed NOTIFYs

---

## API Changes

### New config field in `ReliableDeliveryConfig`

```rust
/// Maximum time to wait for a sequence gap to fill before assuming the
/// transaction was rolled back. When a gap in global_sequence numbers is
/// detected (e.g., event N+1 exists but N doesn't), the system waits up
/// to this duration for event N to appear. After the timeout, the gap is
/// assumed to be from a rolled-back transaction and the checkpoint advances
/// past it.
///
/// Default: 5 seconds
///
/// Increase this value if your system has long-running transactions that
/// write events. Decrease for faster recovery from rolled-back transactions.
pub gap_timeout: Duration,
```

Default value: `Duration::from_secs(5)`

---

## Implementation Phases

Each phase follows a TDD cycle: write failing test(s), implement to pass, refactor.

### Phase 1: `SubscriberState` and `advance_contiguous_checkpoint` (pure logic)

**Files:** `epoch_pg/src/event_bus/mod.rs` (new struct + function, no wiring yet)

**Changes:**
1. Add `SubscriberState` struct with `contiguous_checkpoint`, `processed_ahead: HashSet<u64>`, `gap_first_seen: HashMap<u64, Instant>`
2. Add `SubscriberState::new(checkpoint: u64)` constructor
3. Add `advance_contiguous_checkpoint(state: &mut SubscriberState, visible_seqs: &BTreeSet<u64>, gap_timeout: Duration)` as a pure function

**Tests (unit, in `mod.rs` or a dedicated `subscriber_state.rs` test module):**
- `test_advance_contiguous_no_gaps` — checkpoint=5, visible=[6,7,8] → advances to 8
- `test_advance_contiguous_with_gap` — checkpoint=5, visible=[7,8] → stays at 5, records gap at 6
- `test_advance_contiguous_gap_timeout` — checkpoint=5, visible=[7,8], gap at 6 aged past timeout → advances to 8
- `test_advance_contiguous_with_processed_ahead` — checkpoint=5, processed_ahead={6}, visible=[7,8] → advances to 8
- `test_advance_contiguous_multiple_gaps` — checkpoint=5, visible=[8,10], processed_ahead={} → stays at 5, records gaps at 6,7
- `test_subscriber_state_new_defaults` — verify empty sets and correct checkpoint

**Verification:** `cargo test` — unit tests pass. No integration behavior changes yet.

---

### Phase 2: `gap_timeout` config field

**Files:** `epoch_pg/src/event_bus/config.rs`

**Changes:**
1. Add `pub gap_timeout: Duration` to `ReliableDeliveryConfig`
2. Update `Default` impl: `gap_timeout: Duration::from_secs(5)`

**Tests (unit, in `config.rs`):**
- `test_default_config_has_gap_timeout` — verify default is 5s
- `test_custom_gap_timeout` — verify custom value is stored

**Verification:** `cargo test` — existing config tests still pass, new tests pass.

---

### Phase 3: Replace listener loop — NOTIFY as wake-up signal

**Files:** `epoch_pg/src/event_bus/mod.rs` (the `start_listener` spawned task)

This is the core behavioral change. Replace the current NOTIFY→deserialize→process flow with NOTIFY→DB query→process flow.

**Changes:**
1. Replace `checkpoint_cache: HashMap<String, u64>` with `subscriber_states: HashMap<String, SubscriberState>`
2. On NOTIFY received (the `Ok(notification)` arm):
   - Log the notification at debug level (payload ignored for processing)
   - For each subscriber, call a new helper `process_pending_events_for_subscriber()` that:
     a. Gets/initializes `SubscriberState` from `subscriber_states` (DB lookup on miss, same as current cache miss logic)
     b. Queries `epoch_events WHERE global_sequence > contiguous_checkpoint ORDER BY global_sequence ASC LIMIT catch_up_batch_size`
     c. For each event: skip if in `processed_ahead`; attempt deserialization (same error handling as current); call `process_event_with_retry`; insert into `processed_ahead`
     d. Call `advance_contiguous_checkpoint`
     e. Update `pending_checkpoints` using `contiguous_checkpoint` (not the event's sequence)
     f. Flush checkpoint if appropriate (same `should_flush_checkpoint` logic)
     g. Loop if batch was full
3. Remove the current event deserialization from NOTIFY payload (the `PgDBEvent` deserialization, `Event` construction, and per-subscriber checkpoint check + process)
4. Update the deserialization-failure path (currently advances checkpoint for all subscribers) — no longer needed since events are fetched from DB per-subscriber

**Tests (integration, in `pgeventbus_integration_tests.rs`):**
- `test_out_of_order_notify_both_events_processed` — **the bug reproduction test:**
  - Subscribe a projection
  - Use two concurrent `sqlx::Transaction`s to create out-of-order commits:
    - Tx A: `nextval()` → N, sleep 200ms, INSERT, COMMIT
    - Tx B: `nextval()` → N+1, INSERT, COMMIT immediately
  - Wait for processing
  - Assert both events were received by the projection
- Existing tests must still pass: `test_subscribe_and_event_propagation`, `test_multiple_subscribers`, `test_event_bus_notification_includes_global_sequence`, `test_checkpoint_updated_after_successful_event_processing`

**Verification:** `cargo test` — new integration test passes (would have failed with old code), all existing tests pass.

---

### Phase 4: Periodic timer triggers event processing (gap resolution)

**Files:** `epoch_pg/src/event_bus/mod.rs` (the `sleep(flush_interval)` arm in `tokio::select!`)

**Changes:**
1. In the periodic timer branch (currently only flushes expired checkpoints), also run `process_pending_events_for_subscriber()` for all subscribers
2. This ensures gaps from delayed commits are filled, and timed-out gaps (rolled-back transactions) are advanced past

**Tests (integration):**
- `test_rolled_back_transaction_gap_resolved` — 
  - Configure `gap_timeout` to 1 second (short for testing)
  - Store event with seq N
  - Begin transaction, obtain seq N+1 via `SELECT nextval(...)`, ROLLBACK
  - Store event with seq N+2
  - Wait for gap timeout + periodic tick
  - Assert both N and N+2 were processed, and checkpoint advanced past the gap

**Verification:** `cargo test` — gap timeout test passes, existing tests still pass.

---

### Phase 5: Regression and stress tests

**Files:** `epoch_pg/tests/pgeventbus_integration_tests.rs`

No new production code changes. Add tests to verify existing behavior is preserved.

**Tests (integration):**
- `test_burst_concurrent_events_all_processed` — store 50 events via concurrent tasks, verify all received
- `test_multiple_subscribers_out_of_order` — two subscribers, out-of-order events, both receive all
- `test_catchup_plus_realtime_handoff_no_loss` — events before and after subscribe, verify no gaps
- `test_batched_checkpoint_with_gap_tracking` — batched mode still flushes correctly with new state tracking
- `test_graceful_shutdown_flushes_subscriber_states` — shutdown persists correct contiguous checkpoints
- `test_undeserializable_event_advances_past` — malformed event in DB, verify checkpoint advances

**Verification:** `cargo test` — full test suite green. `cargo clippy -- -D warnings` clean. `cargo fmt --check` clean.

---

## Migration Considerations

- No database schema changes required
- No new migrations needed  
- The `gap_timeout` config field has a sensible default — existing callers using `ReliableDeliveryConfig::default()` get the fix automatically
- Existing checkpoint data is compatible — `SubscriberState` is initialized from the persisted checkpoint on startup

---

## Related Specs

- [0002-pg-event-bus-reliability.md](0002-pg-event-bus-reliability.md) — original reliability improvements that introduced the checkpoint cache
- [0005-checkpoint-mode-batched.md](0005-checkpoint-mode-batched.md) — batched checkpoint flushing (compatible with this fix)
- [0011-inmemory-eventbus-async-dispatch.md](0011-inmemory-eventbus-async-dispatch.md) — `InMemoryEventBus` is not affected (uses channels, not NOTIFY)
