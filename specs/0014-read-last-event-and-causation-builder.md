# Specification: Read Last Event and Causation Builder

**Document ID:** 0014
**Status:** Proposed
**Created:** 2026-02-22
**Author:** AI Agent
**Motivation:** Catacloud event timeline viewer (Phase 3) highlights the need for
framework-level helpers to replace raw SQL workarounds in background tasks.

---

## Problem Statement

Catacloud background tasks (18 call sites across 8 files) need to look up an
entity's last event metadata to inherit correlation/causation context for
recovery commands. The current workaround uses raw SQL:

```rust
// core/src/tasks/mod.rs — get_entity_correlation()
sqlx::query_as::<_, EventRow>(
    "SELECT id, correlation_id FROM epoch_events
     WHERE stream_id = $1 ORDER BY global_sequence DESC LIMIT 1"
)
.bind(entity_id)
.fetch_optional(postgres)
.await
```

This bypasses the framework, is Postgres-specific, and duplicates knowledge of
the `epoch_events` table schema.

Additionally, `Command` provides `.caused_by(&event)` which requires a full
`Event` reference, but background tasks often only have an event ID (from the
metadata query). There is no `Command::with_causation_id(uuid)` builder.

## Proposed API

See `0013-event-metadata-query-helpers.md` for full API specification.
This spec serves as the implementation tracking document.

### Summary of Required Changes

1. **`EventStoreBackend::read_last_event(stream_id) -> Option<Event>`**
   - PostgreSQL: `SELECT ... ORDER BY global_sequence DESC LIMIT 1`
   - InMemory: `self.events.get(&stream_id).and_then(|v| v.last().cloned())`

2. **`Command::with_causation_id(uuid) -> Self`**
   - Sets `self.causation_id = Some(causation_id)`
   - Complement to `.caused_by(&event)` for metadata-only scenarios

3. **(Optional) `EventStoreBackend::read_last_event_metadata(stream_id) -> Option<EventMetadata>`**
   - Lightweight alternative avoiding JSONB deserialization
   - Lower priority; `read_last_event()` is sufficient for current needs

## Catacloud Migration Path

After implementation, Catacloud replaces `get_entity_correlation()` with:

```rust
// Recovery tasks: thread both correlation and causation
if let Some(last_event) = event_store.read_last_event(machine.id).await? {
    let cmd = Command::new(pool_id, ForceReleaseMachine { ... }, creds, version)
        .caused_by(&last_event);
    machine_pool_aggregate.handle(cmd).await?;
}

// Periodic tasks: thread correlation only
if let Some(last_event) = event_store.read_last_event(org_id).await? {
    let correlation_id = last_event.correlation_id.unwrap_or(last_event.id);
    let cmd = Command::new(ledger_id, RecordUsage { ... }, creds, None)
        .with_correlation_id(correlation_id);
    billing_ledger.handle(cmd).await?;
}
```

This eliminates the raw SQL helper, makes the code backend-agnostic,
and uses the framework's own serialization/deserialization.

## Implementation Checklist

- [ ] Add `read_last_event()` to `EventStoreBackend` trait
- [ ] Implement for `PgEventStore`
- [ ] Implement for `InMemoryEventStore`
- [ ] Add `with_causation_id()` to `Command`
- [ ] Add integration tests for both backends
- [ ] Update event-correlation-causation example
- [ ] Bump version

## Related Documents

- [0013 - Event Metadata Query Helpers](./0013-event-metadata-query-helpers.md) — Full API spec
- [Epoch Correlation Spec](./2602121055_spec_event_correlation_and_causation.md) — Existing correlation/causation implementation
- Catacloud `docs/2602221112_spec_event_timeline_viewer.typ` — Motivating use case
