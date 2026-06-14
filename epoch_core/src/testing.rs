//! Reusable contract tests for backend implementations.
//!
//! Gated behind the `testing` feature. Backend authors add
//! `epoch_core = { path = "..", features = ["testing"] }` to their `[dev-dependencies]`
//! and call these helpers from their own test suite.

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
