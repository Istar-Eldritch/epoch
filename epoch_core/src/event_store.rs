//! This module defines the `EventStoreBackend` trait, which provides an interface for interacting
//! with an event store. It includes methods for reading events from a stream and appending new
//! events to a stream. This module also defines the `EventBus` trait, which allows for publishing
//! and subscribing to events.

use crate::event::{Event, EventData};
use async_trait::async_trait;
use futures_core::Stream;
use std::future::Future;
use std::pin::Pin;
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
    fn publish<'a>(
        &'a self,
        event: Event<Self::EventType>,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>>;

    /// Allows to subscribe to events
    fn subscribe<T>(
        &self,
        projector: T,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send>>
    where
        T: EventObserver<Self::EventType> + Send + Sync + 'static;
}

///  Traits to defined observers to the event bus
#[async_trait]
pub trait EventObserver<ED>: Send + Sync
where
    ED: EventData + Send + Sync,
{
    /// Reacts to events published in an event bus
    async fn on_event(
        &self,
        event: Event<ED>,
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
        // SAFETY: `idx` is a `usize` and is therefore `Unpin`.
        // Moving `SliceEventStream` would not invalidate the pointer to `idx`
        // relative to the struct's memory layout.
        // We are only projecting to a field that is itself `Unpin
        if self.idx < self.inner.len() {
            let res = Poll::Ready(Some(Ok(self.inner[self.idx].clone())));
            unsafe { self.as_mut().get_unchecked_mut().idx += 1 };
            return res;
        }
        Poll::Ready(None)
    }
}

impl<'a, D, E> EventStream<D, E> for SliceEventStream<'a, D, E>
where
    D: EventData + Send + Sync + 'a,
    E: std::error::Error + Send + Sync,
{
}
