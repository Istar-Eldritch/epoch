//! Reusable contract tests for backend implementations.
//!
//! Gated behind the `testing` feature. Backend authors add
//! `epoch_core = { path = "..", features = ["testing"] }` to their `[dev-dependencies]`
//! and call these helpers from their own test suite.
//!
//! When the `upcasting` feature is also enabled, the additional
//! [`verify_upcasting_chain`] helper is available for asserting that a
//! registered multi-step upcaster chain correctly transforms a v1 JSON payload
//! all the way to the current schema version and deserializes it into the domain
//! type.

use crate::event::Event;
use crate::event_store::EventStoreBackend;
use uuid::Uuid;

/// Asserts that a backend's [`EventStoreBackend::store_events`] honours the
/// all-or-nothing atomicity contract (see the trait docs).
///
/// The caller supplies:
/// - a freshly-constructed, empty `backend`, and
/// - `make_event(stream_id, stream_version)` producing a valid event for the
///   backend's `EventType`.
///
/// The helper:
/// 1. seeds version 1 with a single `store_event`,
/// 2. submits a batch `[v2, v3, v3]` whose third event duplicates `v3`,
/// 3. asserts the batch is rejected (`Err`), and
/// 4. asserts the stream still contains exactly the one seeded event — proving
///    no event from the failed batch was persisted.
///
/// It additionally verifies the empty-batch no-op and a fully-valid batch.
///
/// # Panics
///
/// Panics with a descriptive message on any contract violation, so it reads as a
/// drop-in `#[tokio::test]` body.
pub async fn verify_store_events_atomicity<B>(
    backend: B,
    make_event: impl Fn(Uuid, u64) -> Event<B::EventType>,
) where
    B: EventStoreBackend,
{
    use tokio_stream::StreamExt;

    // --- empty batch is a no-op ---
    backend
        .store_events(Vec::new())
        .await
        .expect("empty batch must return Ok");

    // --- mid-batch failure must persist nothing from the batch ---
    let stream_id = Uuid::new_v4();
    backend
        .store_event(make_event(stream_id, 1))
        .await
        .expect("seeding version 1 must succeed");

    let bad_batch = vec![
        make_event(stream_id, 2), // ok
        make_event(stream_id, 3), // ok
        make_event(stream_id, 3), // duplicate of stream_version, 3 -> must abort whole batch
    ];
    let result = backend.store_events(bad_batch).await;
    assert!(
        result.is_err(),
        "store_events with a mid-batch duplicate stream_version must return Err"
    );

    let mut stream = backend
        .read_events(stream_id)
        .await
        .expect("read_events must succeed");
    let mut count = 0usize;
    while let Some(item) = stream.next().await {
        item.expect("stream item must not error");
        count += 1;
    }
    assert_eq!(
        count, 1,
        "after a rejected batch the stream must contain only the seeded event \
         (non-atomic backend leaked partial writes)"
    );

    // --- a fully-valid batch is persisted in full ---
    let good_batch = vec![make_event(stream_id, 2), make_event(stream_id, 3)];
    backend
        .store_events(good_batch)
        .await
        .expect("valid batch must succeed");

    let mut stream = backend.read_events(stream_id).await.unwrap();
    let mut count = 0usize;
    while let Some(item) = stream.next().await {
        item.unwrap();
        count += 1;
    }
    assert_eq!(count, 3, "valid batch must append all events");
}

// ─── Upcasting contract helper ───────────────────────────────────────────────

#[cfg(feature = "upcasting")]
use crate::event::EventData;
#[cfg(feature = "upcasting")]
use crate::upcasting::UpcasterRegistry;

/// Asserts that an [`UpcasterRegistry`] correctly applies a multi-step chain
/// and deserializes the result into the domain type.
///
/// Gated behind the `upcasting` feature. Enable it alongside `testing` in
/// your `[dev-dependencies]`:
///
/// ```toml
/// epoch_core = { path = "..", features = ["testing", "upcasting"] }
/// ```
///
/// # What the caller must supply
///
/// - `registry` — a fully configured [`UpcasterRegistry`] with **at least one**
///   upcaster step registered for `event_type`. The helper assumes the stored
///   payload is at version `1`.
/// - `event_type` — the event type string the chain targets (PascalCase,
///   matching [`EventData::event_type`]).
/// - `v1_payload` — a [`serde_json::Value`] payload in the oldest stored
///   schema version (`1`).
/// - `expected` — the domain value the caller expects after all upcast steps
///   are applied and the JSON is deserialized into `D`.
///
/// # Contract assertions
///
/// 1. **Multi-step happy path**: `upcast_and_deserialize` from
///    `stored_version = 1` returns `Ok(Some(expected_value))`.
/// 2. **Applied counter**: `counters().applied()` equals
///    `current_version − 1` (one step applied per version increment).
/// 3. **Succeeded counter**: `counters().succeeded()` is `1`.
/// 4. **Purged event** (`None` payload): `upcast_and_deserialize` with
///    `None` returns `Ok(None)` and does **not** increment `succeeded` or
///    `failed`.
///
/// # Panics
///
/// Panics with a descriptive message on any contract violation, so it reads as
/// a drop-in `#[tokio::test]` body.
#[cfg(feature = "upcasting")]
pub async fn verify_upcasting_chain<D>(
    registry: &UpcasterRegistry,
    event_type: &str,
    v1_payload: serde_json::Value,
    expected: D,
) where
    D: EventData + PartialEq + std::fmt::Debug,
{
    // --- 1. Multi-step happy path ------------------------------------------
    let stream_id = Uuid::new_v4();
    let event_id = Uuid::new_v4();

    let result: Result<Option<D>, crate::upcasting::UpcastError> = registry
        .upcast_and_deserialize(event_type, 1, stream_id, event_id, Some(v1_payload.clone()))
        .await;

    assert!(
        result.is_ok(),
        "upcast_and_deserialize must succeed for a valid v1 payload, got: {:?}",
        result
    );
    let deserialized = result.unwrap();
    assert!(
        deserialized.is_some(),
        "upcast_and_deserialize must return Some(D) for a non-None payload"
    );
    assert_eq!(
        deserialized.unwrap(),
        expected,
        "upcast_and_deserialize must produce the expected deserialized value"
    );

    // --- 2 & 3. Counter assertions after the successful round-trip -----------
    let current_version = registry.current_version(event_type);
    let expected_steps = (current_version.saturating_sub(1)) as u64;
    assert_eq!(
        registry.counters().applied(),
        expected_steps,
        "counters().applied() must equal current_version − 1 ({expected_steps} step(s)) \
         after upcasting from v1 to v{current_version}"
    );
    assert_eq!(
        registry.counters().succeeded(),
        1,
        "counters().succeeded() must be 1 after one successful upcast"
    );
    assert_eq!(
        registry.counters().failed(),
        0,
        "counters().failed() must be 0 after a successful upcast"
    );

    // --- 4. Purged event (None payload) → Ok(None), counters unchanged -------
    let succeeded_before = registry.counters().succeeded();
    let failed_before = registry.counters().failed();

    let purged_result: Result<Option<D>, crate::upcasting::UpcastError> = registry
        .upcast_and_deserialize(event_type, 1, Uuid::new_v4(), Uuid::new_v4(), None)
        .await;

    assert!(
        matches!(purged_result, Ok(None)),
        "upcast_and_deserialize with a None payload must return Ok(None) (purged event), \
         got: {:?}",
        purged_result
    );
    assert_eq!(
        registry.counters().succeeded(),
        succeeded_before,
        "purged event must not increment the succeeded counter"
    );
    assert_eq!(
        registry.counters().failed(),
        failed_before,
        "purged event must not increment the failed counter"
    );
}
