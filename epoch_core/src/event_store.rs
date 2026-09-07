//! This module defines the `EventStoreBackend` trait, which provides an interface for interacting
//! with an event store. It includes methods for reading events from a stream and appending new
//! events to a stream. This module also defines the `EventBus` trait, which allows for publishing
//! and subscribing to events.

use crate::event::{Event, EventData};
use async_trait::async_trait;
use futures_core::Stream;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use uuid::Uuid;

/// A trait that defines the behavior of an event stream.
pub trait EventStream<D, E>: Stream<Item = Result<Event<D>, E>> + Send
where
    D: EventData + Send + Sync,
{
}

/// A trait that defines the behavior of a storage backend.
#[async_trait]
pub trait EventStoreBackend: Send + Sync {
    /// The type of event stored in this store
    type EventType: EventData + Send + Sync;

    /// The error when an event store operation fails
    type Error: std::error::Error + Send + Sync;

    /// Fetches an inclusive `[from, to]` `stream_version` range from the backend.
    ///
    /// `from` and `to` are inclusive `stream_version` bounds; `None` means unbounded on
    /// that side. Events are yielded in ascending `stream_version` order. An empty range
    /// (`from > to`) yields zero events and is not an error.
    ///
    /// This is the bounded-replay primitive: [`read_events`](Self::read_events) and
    /// [`read_events_since`](Self::read_events_since) are default methods that delegate here.
    /// Backends should push the bounds down to storage rather than over-reading and filtering.
    async fn read_events_range(
        &self,
        stream_id: Uuid,
        from: Option<u64>,
        to: Option<u64>,
    ) -> Result<Pin<Box<dyn EventStream<Self::EventType, Self::Error> + Send + 'life0>>, Self::Error>;

    /// Fetches the full stream from the storage backend.
    ///
    /// Default implementation delegates to
    /// [`read_events_range`](Self::read_events_range) with both bounds unset.
    async fn read_events(
        &self,
        stream_id: Uuid,
    ) -> Result<Pin<Box<dyn EventStream<Self::EventType, Self::Error> + Send + 'life0>>, Self::Error>
    {
        self.read_events_range(stream_id, None, None).await
    }

    /// Fetches the stream from `version` (inclusive) onward.
    ///
    /// Default implementation delegates to
    /// [`read_events_range`](Self::read_events_range) with `from = Some(version)` and no upper bound.
    async fn read_events_since(
        &self,
        stream_id: Uuid,
        version: u64,
    ) -> Result<Pin<Box<dyn EventStream<Self::EventType, Self::Error> + Send + 'life0>>, Self::Error>
    {
        self.read_events_range(stream_id, Some(version), None).await
    }

    /// Appends events to a stream.
    async fn store_event(&self, event: Event<Self::EventType>) -> Result<(), Self::Error>;

    /// Appends multiple events to one or more streams **atomically**.
    ///
    /// # Atomicity Contract
    ///
    /// Implementations **must** guarantee all-or-nothing persistence:
    ///
    /// - If every event is persisted successfully, the call returns `Ok(())`.
    /// - If **any** event fails to persist (most commonly a `stream_version`
    ///   optimistic-concurrency conflict mid-batch), the call returns `Err(..)` and
    ///   **no event from the batch is persisted** — a subsequent [`read_events`]
    ///   must observe the store exactly as it was before this call.
    /// - Per-stream `stream_version` monotonicity must be preserved; a batch that
    ///   would create a gap or conflict for any stream must be rejected in full.
    /// - An empty batch is a no-op that returns `Ok(())`.
    ///
    /// Events are published to the event bus **only after** successful, durable
    /// persistence. Publishing is best-effort: a bus failure after the commit does
    /// **not** roll back the persisted events (projections recover by replay).
    ///
    /// # Verifying Your Implementation
    ///
    /// Use `crate::testing::verify_store_events_atomicity` in your backend's test
    /// suite to assert this contract holds.
    ///
    /// [`read_events`]: Self::read_events
    async fn store_events(&self, events: Vec<Event<Self::EventType>>) -> Result<(), Self::Error>;

    /// Durably persists `events` WITHOUT publishing them to the bus, returning the
    /// stored events (backends that assign identifiers such as `global_sequence`
    /// return the enriched copies) for a later [`publish_stored_events`](Self::publish_stored_events) call.
    ///
    /// This is the persist half of the fused [`store_events`](Self::store_events).
    /// `Aggregate::handle()` uses it to persist state before publishing, so a
    /// synchronous Inline subscriber that reenters the same aggregate observes
    /// durable state.
    ///
    /// # Default
    ///
    /// Delegates to [`store_events`](Self::store_events) (which persists AND
    /// publishes) and echoes the input events back. Backends that do not override
    /// this keep today's publish-before-persist ordering (acceptable: they are not
    /// on the reentrant Inline write path). Production backends override it to
    /// genuinely split persist from publish.
    async fn store_events_without_publish(
        &self,
        events: Vec<Event<Self::EventType>>,
    ) -> Result<Vec<Event<Self::EventType>>, Self::Error> {
        self.store_events(events.clone()).await?;
        Ok(events)
    }

    /// Publishes already-durable `events` to the bus. Pairs with
    /// [`store_events_without_publish`](Self::store_events_without_publish).
    ///
    /// # Default
    ///
    /// No-op (`Ok(())`). Combined with the [`store_events_without_publish`](Self::store_events_without_publish)
    /// default — which already published inside [`store_events`](Self::store_events) —
    /// this keeps non-overriding backends byte-identical.
    async fn publish_stored_events(
        &self,
        _events: Vec<Event<Self::EventType>>,
    ) -> Result<(), Self::Error> {
        Ok(())
    }

    /// Returns the most recent event in the given stream, or `None` if the stream is empty.
    ///
    /// "Most recent" is defined as the event with the highest `stream_version` — i.e. the
    /// last event that was appended to the stream identified by `stream_id`.
    ///
    /// Returns `Ok(None)` when the stream does not exist or contains no events.
    ///
    /// # Use Case
    ///
    /// This is useful for background tasks (e.g. a resuming process manager) that need to
    /// re-link into an existing causation/correlation tree without re-reading the entire
    /// stream. Pair the returned event's `id` with
    /// [`Command::with_causation_id`](crate::aggregate::Command::with_causation_id) to
    /// continue an existing causal chain.
    ///
    /// # Default Implementation
    ///
    /// The default implementation drains the stream returned by
    /// [`read_events`](Self::read_events) and keeps the last yielded event. This is an
    /// **O(N)** operation in the number of events in the stream. Backends that can answer
    /// this query directly — for example via an indexed
    /// `ORDER BY stream_version DESC LIMIT 1` query — **should override** this method to
    /// provide O(1) performance.
    async fn read_last_event(
        &self,
        stream_id: Uuid,
    ) -> Result<Option<Event<Self::EventType>>, Self::Error> {
        let mut stream = self.read_events(stream_id).await?;
        let mut last = None;
        while let Some(item) = std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
            last = Some(item?);
        }
        Ok(last)
    }

    /// Returns all events sharing the given correlation ID, ordered by global sequence.
    ///
    /// This is a cross-stream query — it searches across all event streams for events
    /// tagged with the specified `correlation_id`. Use this to reconstruct the full
    /// picture of everything that happened as a result of a single user action or
    /// external trigger.
    ///
    /// Returns an empty `Vec` if no events match the given correlation ID.
    async fn read_events_by_correlation_id(
        &self,
        correlation_id: Uuid,
    ) -> Result<Vec<Event<Self::EventType>>, Self::Error>;

    /// Returns the causal subtree for the given event.
    ///
    /// The subtree includes the event's ancestors (walking up via `causation_id`),
    /// the event itself, and all descendants — excluding unrelated branches that
    /// share the same correlation ID but are not in the direct causal path.
    ///
    /// Events are ordered by global sequence.
    ///
    /// Returns an empty `Vec` if the event is not found.
    async fn trace_causation_chain(
        &self,
        event_id: Uuid,
    ) -> Result<Vec<Event<Self::EventType>>, Self::Error>;
}

/// A trait that defines the behavior of an event bus.
pub trait EventBus {
    /// The type of event that can be published to this event bus.
    type EventType: EventData + Send + Sync + 'static;
    /// The errors when an event bus operation fails
    type Error: std::error::Error;
    /// Publishes events to the event bus.
    ///
    /// Events are wrapped in `Arc` to allow efficient sharing across multiple observers
    /// without requiring deep clones. Each observer receives an `Arc::clone()` which is
    /// an O(1) atomic increment operation.
    fn publish<'a>(
        &'a self,
        event: Arc<Event<Self::EventType>>,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>>;

    /// Allows to subscribe to events
    fn subscribe<T>(
        &self,
        projector: T,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send>>
    where
        T: EventObserver<Self::EventType> + Send + Sync + 'static;
}

/// How a subscriber reacts when an event cannot be applied.
///
/// Controls the bus's behaviour at every failure point (deserialisation error,
/// observer error, unproven gap) for a given subscriber.
///
/// # Default
///
/// The default is [`FailureMode::FailOpen`]: on any failure the bus logs, skips
/// the event, and advances the checkpoint past it. This is the correct choice
/// for most subscribers.
///
/// # `FailClosed` and frozen read models
///
/// When a subscriber opts into [`FailureMode::FailClosed`], the bus halts
/// delivery at the first unrecoverable failure: the subscriber's checkpoint is
/// held below the bad event and no further events are applied to *that
/// subscriber* until the condition is resolved (the bad row is fixed, or an
/// operator calls `release_halt`). The halt is **subscriber-local**: healthy
/// subscribers keep advancing because a wedged subscriber is excluded from the
/// shared event-window floor and served by its own private re-seeding fetch,
/// so it cannot pin the delivery window of any healthy peer.
///
/// **Cross-group consequence (R12):** while the halted subscriber's read model
/// is frozen, later priority groups keep running against that stale view. For a
/// deny-heavy oracle (e.g. an auth projection), stale data errs on the side of
/// denying rather than over-admitting — the freeze fails safe. Recovery is
/// self-healing: once the cause is fixed, the held event and everything after
/// it are applied exactly once in order on the next batch cycle. For a gap
/// that never resolves, an operator release advances the subscriber's persisted
/// position past the held sequence. A permanent unreleased halt means a
/// permanently frozen read model for that subscriber; it does not starve or
/// slow any other subscriber on the bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum FailureMode {
    /// Default. On any delivery failure, log, skip, and advance the checkpoint
    /// past the failed event. The read model may silently diverge from the
    /// event log, but delivery continues unimpeded.
    #[default]
    FailOpen,
    /// Halt delivery at the first unrecoverable failure. The checkpoint is held
    /// below the bad event; no further events are applied until the condition
    /// clears. See the [`FailureMode`] docs for the cross-group wedge risk.
    FailClosed,
}

/// How a subscriber relates to persisted checkpoints.
///
/// Controls whether the event bus reads and writes a checkpoint row for this
/// subscriber, or tracks progress via a per-process in-memory high-water mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum SubscriptionMode {
    /// Default. Honour and advance a persisted checkpoint; resume from it across
    /// process restarts.
    #[default]
    Checkpointed,
    /// Replay from sequence 0 on every process start; never read or write a
    /// persisted checkpoint. Readiness is tracked via a per-process in-memory
    /// high-water mark. For in-memory read models with no durable store.
    ///
    /// # Caveat: re-subscribing does not rebuild your model for you
    /// A backend that implements this mode resets its own high-water mark to 0
    /// on every `subscribe()` call, including calling `subscribe()` again with
    /// a `subscriber_id` already in use, so catch-up replays the full history
    /// again. If you pass in an observer that wraps a model instance which
    /// **survived** from a previous subscription (rather than a fresh one),
    /// that model will have the same history applied to it twice. Build a
    /// fresh model before every `subscribe()` call for a `ReplayAlways`
    /// subscriber, or ensure `apply` is idempotent under re-application.
    ReplayAlways,
}

/// Traits to define observers to the event bus.
///
/// Observers receive events wrapped in `Arc` for efficient sharing. Observers can:
/// - Dereference for read-only access (zero cost)
/// - Clone the `Arc` if they need to retain the event (cheap O(1) operation)
/// - Pass `&*event` to functions expecting `&Event<ED>` (zero cost)
///
/// # Subscriber ID
///
/// Implementors must also implement [`SubscriberId`](crate::SubscriberId) to provide
/// a unique identifier for checkpoint tracking, multi-instance coordination, and
/// dead letter queue association. Use the `#[derive(SubscriberId)]` macro from
/// `epoch_derive` for automatic implementation.
#[async_trait]
pub trait EventObserver<ED>: crate::SubscriberId + Send + Sync
where
    ED: EventData + Send + Sync,
{
    /// Reacts to events published in an event bus.
    ///
    /// The event is wrapped in `Arc` for efficient sharing across multiple observers.
    async fn on_event(
        &self,
        event: Arc<Event<ED>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Processing priority for event bus ordering.
    ///
    /// Lower values are processed first. Projections default to `0` and sagas
    /// (via [`SagaHandler`](crate::saga::SagaHandler)) default to `100`, ensuring
    /// read models are up-to-date before sagas query them.
    fn priority(&self) -> u8 {
        0
    }

    /// How this subscriber relates to persisted checkpoints.
    ///
    /// Defaults to [`SubscriptionMode::Checkpointed`]: honour and advance a
    /// persisted checkpoint row across process restarts. Override and return
    /// [`SubscriptionMode::ReplayAlways`] for in-memory read models that must
    /// replay from sequence 0 on every process start.
    fn subscription_mode(&self) -> SubscriptionMode {
        SubscriptionMode::Checkpointed
    }

    /// How the bus reacts when this subscriber cannot apply an event.
    ///
    /// Defaults to [`FailureMode::FailOpen`]: log, skip, and advance past the
    /// failed event. Override and return [`FailureMode::FailClosed`] for
    /// subscribers that must never silently diverge from the event log.
    fn failure_mode(&self) -> FailureMode {
        FailureMode::FailOpen
    }
}

/// Used to construct an event stream from a slice.
pub struct SliceEventStream<'a, D, E>
where
    D: EventData + Send + Sync + 'a,
    E: std::error::Error + Send + Sync,
{
    inner: &'a [Event<D>],
    idx: usize,
    _marker: std::marker::PhantomData<(&'a D, E)>,
}

/// A trait that defines the behavior of a reference-based event stream.
///
/// This is an internal optimization for scenarios where events are borrowed
/// from a slice (e.g., in `Aggregate::handle` after generating events from a command).
/// Unlike [`EventStream`], this yields references to events rather than owned events,
/// avoiding unnecessary cloning.
///
/// This trait is not part of the public API and is used internally by the framework.
pub trait RefEventStream<'a, D, E>: Stream<Item = Result<&'a Event<D>, E>> + Send
where
    D: EventData + Send + Sync + 'a,
{
}

/// A reference-based event stream constructed from a slice.
///
/// This is an optimized version of [`SliceEventStream`] that yields references
/// to events instead of cloning them. Used internally by `Aggregate::handle`
/// to avoid cloning freshly created events during re-hydration.
///
/// # Performance
///
/// This eliminates the clone that occurs in [`SliceEventStream`], which is
/// beneficial when events have large payloads. However, the lifetime of the
/// yielded references is tied to the slice, so this can only be used when
/// the slice outlives the stream consumption.
pub struct SliceRefEventStream<'a, D, E>
where
    D: EventData + Send + Sync + 'a,
    E: std::error::Error + Send + Sync,
{
    inner: &'a [Event<D>],
    idx: usize,
    _marker: std::marker::PhantomData<E>,
}

impl<'a, D, E> From<&'a [Event<D>]> for SliceEventStream<'a, D, E>
where
    D: EventData + Send + Sync + 'a,
    E: std::error::Error + Send + Sync,
{
    fn from(events: &'a [Event<D>]) -> Self {
        Self {
            inner: events,
            idx: 0,
            _marker: std::marker::PhantomData,
        }
    }
}

// All fields (slice reference, usize, PhantomData) are Unpin, so SliceEventStream is Unpin.
impl<'a, D, E> Unpin for SliceEventStream<'a, D, E>
where
    D: EventData + Send + Sync + 'a,
    E: std::error::Error + Send + Sync,
{
}

impl<'a, D, E> Stream for SliceEventStream<'a, D, E>
where
    D: EventData + Send + Sync + 'a,
    E: std::error::Error + Send + Sync,
{
    type Item = Result<Event<D>, E>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        if this.idx < this.inner.len() {
            let event = this.inner[this.idx].clone();
            this.idx += 1;
            Poll::Ready(Some(Ok(event)))
        } else {
            Poll::Ready(None)
        }
    }
}

impl<'a, D, E> EventStream<D, E> for SliceEventStream<'a, D, E>
where
    D: EventData + Send + Sync + 'a,
    E: std::error::Error + Send + Sync,
{
}

impl<'a, D, E> From<&'a [Event<D>]> for SliceRefEventStream<'a, D, E>
where
    D: EventData + Send + Sync + 'a,
    E: std::error::Error + Send + Sync,
{
    fn from(events: &'a [Event<D>]) -> Self {
        Self {
            inner: events,
            idx: 0,
            _marker: std::marker::PhantomData,
        }
    }
}

// All fields (slice reference, usize, PhantomData) are Unpin, so SliceRefEventStream is Unpin.
impl<'a, D, E> Unpin for SliceRefEventStream<'a, D, E>
where
    D: EventData + Send + Sync + 'a,
    E: std::error::Error + Send + Sync,
{
}

impl<'a, D, E> Stream for SliceRefEventStream<'a, D, E>
where
    D: EventData + Send + Sync + 'a,
    E: std::error::Error + Send + Sync,
{
    type Item = Result<&'a Event<D>, E>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        if this.idx < this.inner.len() {
            let event = &this.inner[this.idx];
            this.idx += 1;
            Poll::Ready(Some(Ok(event)))
        } else {
            Poll::Ready(None)
        }
    }
}

impl<'a, D, E> RefEventStream<'a, D, E> for SliceRefEventStream<'a, D, E>
where
    D: EventData + Send + Sync + 'a,
    E: std::error::Error + Send + Sync,
{
}
