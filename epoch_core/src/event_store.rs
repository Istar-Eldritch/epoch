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

    /// Fetches a stream from the storage backend.
    async fn read_events(
        &self,
        stream_id: Uuid,
    ) -> Result<Pin<Box<dyn EventStream<Self::EventType, Self::Error> + Send + 'life0>>, Self::Error>;

    /// Fetches a stream from the storage backend.
    async fn read_events_since(
        &self,
        stream_id: Uuid,
        version: u64,
    ) -> Result<Pin<Box<dyn EventStream<Self::EventType, Self::Error> + Send + 'life0>>, Self::Error>;

    /// Appends events to a stream.
    async fn store_event(&self, event: Event<Self::EventType>) -> Result<(), Self::Error>;
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

/// Traits to define observers to the event bus.
///
/// Observers receive events wrapped in `Arc` for efficient sharing. Observers can:
/// - Dereference for read-only access (zero cost)
/// - Clone the `Arc` if they need to retain the event (cheap O(1) operation)
/// - Pass `&*event` to functions expecting `&Event<ED>` (zero cost)
#[async_trait]
pub trait EventObserver<ED>: Send + Sync
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
/// to events instead of cloning them. Used internally by [`Aggregate::handle`]
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
