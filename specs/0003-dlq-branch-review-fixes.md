# Specification: DLQ Branch Review Fixes

**Status:** Implemented  
**Created:** 2026-01-26  
**Updated:** 2026-01-26  
**Author:** AI Agent  

## 1. Overview

This spec addresses the REVIEW comments found in the `dlq` branch code review. All issues have been implemented.

## 2. Issues Addressed

### 2.1 InstanceMode::Coordinated Not Implemented ✅

**Location:** `epoch_pg/src/event_bus.rs`

**Resolution:** Removed `InstanceMode::Coordinated` variant. The enum now only contains `SingleInstance`. The advisory lock helper methods are kept for users who want to implement manual coordination. Added `#[non_exhaustive]` to allow future additions.

### 2.2 CheckpointMode::Batched Not Implemented ✅

**Location:** `epoch_pg/src/event_bus.rs`

**Resolution:** Removed `CheckpointMode::Batched` variant. The enum now only contains `Synchronous`. Added `#[non_exhaustive]` to allow future additions.

### 2.3 Race Condition During Catch-up ✅

**Location:** `epoch_pg/src/event_bus.rs` - `subscribe()` method

**Resolution:** Implemented event buffering during catch-up:
1. A temporary NOTIFY listener is spawned that buffers events during catch-up
2. Catch-up events are processed from the database
3. Buffered events are drained with deduplication (skip if `global_sequence <= checkpoint`)
4. The observer is then added to live projections

### 2.4 Catch-up Doesn't Use Retry/DLQ ✅

**Location:** `epoch_pg/src/event_bus.rs`

**Resolution:** Created `process_event_with_retry()` helper function that encapsulates the retry logic with exponential backoff and DLQ insertion. This function is now used in both catch-up processing and buffered event processing.

### 2.5 Migration m002 NOT NULL Constraint ✅

**Location:** `epoch_pg/src/migrations/m002_add_global_sequence.rs`

**Resolution:** Added `ALTER TABLE events ALTER COLUMN global_sequence SET NOT NULL` after the backfill completes.

### 2.6 Missing Re-exports in lib.rs ✅

**Location:** `epoch_pg/src/lib.rs`

**Resolution:** Added re-exports for `ReliableDeliveryConfig`, `CheckpointMode`, `InstanceMode`, and `DlqEntry`.

### 2.7 Let-chains Syntax Compatibility ✅

**Location:** `epoch_pg/src/event_bus.rs`

**Resolution:** Confirmed project uses `edition = "2024"` which supports let-chains. Removed the REVIEW comment.

## 3. Files Modified

1. `epoch_pg/src/event_bus.rs`:
   - Added `ProcessResult` enum and `process_event_with_retry()` helper
   - Simplified `CheckpointMode` (removed `Batched`)
   - Simplified `InstanceMode` (removed `Coordinated`)
   - Added `#[non_exhaustive]` to both enums
   - Refactored `subscribe()` to implement event buffering during catch-up
   - Applied retry/DLQ logic to catch-up and buffered event processing
   - Removed all REVIEW comments
   - Updated tests

2. `epoch_pg/src/migrations/m002_add_global_sequence.rs`:
   - Added NOT NULL constraint after backfill
   - Removed REVIEW comment

3. `epoch_pg/src/migrations/m004_rename_tables_with_epoch_prefix.rs`:
   - Removed REVIEW comment

4. `epoch_pg/src/lib.rs`:
   - Added re-exports for reliability configuration types
   - Removed REVIEW comment

5. `specs/0002-pg-event-bus-reliability.md`:
   - Updated status to Implemented
   - Added implementation notes
   - Removed REVIEW comment

## 4. Testing

All existing tests pass:
- Unit tests for configuration defaults
- Integration tests for catch-up, checkpointing, and DLQ

## 5. Backward Compatibility

These changes are source-compatible for typical usage:
- Enum variants were removed, but these were never usable anyway (not implemented)
- `#[non_exhaustive]` allows future additions without breaking changes
- All public APIs maintain the same interface
