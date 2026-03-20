# Specification: Event Metadata Query Helpers

**Document ID:** 0013  
**Status:** Proposed  
**Created:** 2026-02-22  
**Author:** AI Agent  
**Motivation:** Catacloud background tasks need to look up an entity's last event metadata (correlation_id, event id) to thread causal context into recovery commands. Currently requires raw SQL against `epoch_events`.

---

## Problem Statement

When a background task (e.g., a stale machine checker) detects an issue with an entity and dispatches a recovery command, it needs to inherit the entity's correlation chain so the recovery event appears in the same timeline as the original user action.

The current API provides:
- `read_events(stream_id)` → returns a **stream** of all events for the entity
- `read_events_by_correlation_id(correlation_id)` → returns all events sharing a correlation ID

Neither is suitable for the common pattern of "give me the last event's metadata so I can thread correlation/causation into a new command":

- `read_events()` returns a stream — you'd consume all N events just to get the last one
- There's no `read_last_event()` or `get_event_metadata()` method
- There's no `Command::with_causation_id(uuid)` builder method — only `.caused_by(&event)` which requires a full `Event` reference

Catacloud works around this with a direct SQL query:

```rust
struct EntityCorrelation {
    last_event_id: Uuid,
    correlation_id: Option<Uuid>,
}

let corr = sqlx::query_as::<_, EntityCorrelation>(
    "SELECT id as last_event_id, correlation_id FROM epoch_events 
     WHERE stream_id = $1 ORDER BY global_sequence DESC LIMIT 1"
)
.bind(entity_id)
.fetch_optional(&postgres)
.await?;
```

This works but bypasses the framework, is Postgres-specific, and duplicates knowledge of the `epoch_events` table schema.

## Proposed API Changes

### 1. `EventStoreBackend::read_last_event(stream_id)`

Returns the most recent event for a stream without loading the full history.

```rust
/// Returns the most recent event for the given stream, or None if the stream has no events.
///
/// This is more efficient than `read_events(stream_id)` when only the latest
/// event is needed (e.g., for threading correlation context into new commands).
async fn read_last_event(
    &self,
    stream_id: Uuid,
) -> Result<Option<Event<Self::EventType>>, Self::Error>;
```

**PostgreSQL implementation:** `SELECT ... FROM epoch_events WHERE stream_id = $1 ORDER BY global_sequence DESC LIMIT 1`

**In-memory implementation:** `self.events.get(&stream_id).and_then(|v| v.last().cloned())`

### 2. `Command::with_causation_id(uuid)`

Builder method to set causation_id without requiring a full Event reference.

```rust
impl<D, C> Command<D, C> {
    /// Explicitly sets a causation ID.
    ///
    /// Use this when you have the causing event's ID but not the full Event
    /// (e.g., from a metadata-only query). For full Event references, prefer
    /// `.caused_by(&event)` which sets both causation_id and correlation_id.
    pub fn with_causation_id(mut self, causation_id: Uuid) -> Self {
        self.causation_id = Some(causation_id);
        self
    }
}
```

**Note:** `.caused_by(&event)` remains the preferred API when you have the full event. `with_causation_id()` is for cases where only the event ID is available (e.g., from `read_last_event()` or a metadata query).

### 3. (Optional) `EventStoreBackend::read_last_event_metadata(stream_id)`

If deserializing the full event data is considered wasteful for the metadata-only use case, an alternative lighter method could return only the metadata fields:

```rust
pub struct EventMetadata {
    pub id: Uuid,
    pub stream_id: Uuid,
    pub event_type: String,
    pub correlation_id: Option<Uuid>,
    pub causation_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub global_sequence: Option<u64>,
}

async fn read_last_event_metadata(
    &self,
    stream_id: Uuid,
) -> Result<Option<EventMetadata>, Self::Error>;
```

This avoids deserializing the `data` JSONB column. However, `read_last_event()` is likely sufficient for most use cases — a single row deserialization is not expensive.

## Usage Example

With the proposed API, the Catacloud background task pattern becomes:

```rust
// Recovery task: thread both correlation and causation
if let Some(last_event) = event_store.read_last_event(machine.id).await? {
    let cmd = Command::new(pool_id, ForceReleaseMachine { ... }, creds, version)
        .caused_by(&last_event);
    machine_pool_aggregate.handle(cmd).await?;
}

// Periodic task: thread correlation only (no causal relationship)
if let Some(last_event) = event_store.read_last_event(org_id).await? {
    let correlation_id = last_event.correlation_id.unwrap_or(last_event.id);
    let cmd = Command::new(ledger_id, RecordUsage { ... }, creds, None)
        .with_correlation_id(correlation_id);
    billing_ledger.handle(cmd).await?;
}
```

## Backward Compatibility

- `read_last_event()` is a new trait method — all implementations must be updated (PgEventStore, InMemoryEventStore)
- `with_causation_id()` is additive — no breaking changes
- Existing code continues to work unchanged

## Implementation Notes

- `read_last_event()` should use `ORDER BY global_sequence DESC LIMIT 1` (not `stream_version`) to match the ordering semantics of other query methods
- The method should handle empty streams gracefully (return `Ok(None)`)
- Both PgEventStore and InMemoryEventStore need implementations
- Add integration tests for both backends

## Alternatives Considered

- **Expose `PgPool` from `PgEventStore`**: Allows raw SQL but breaks abstraction
- **Add `read_events_reversed(stream_id, limit)` stream**: More general but overengineered for this use case
- **Do nothing**: Consumers write raw SQL (current Catacloud approach) — works but bypasses the framework
