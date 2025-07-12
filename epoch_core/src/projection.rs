//! This module defines traits for `Projection`
//! A `Projection` is a read-model built from a stream of events.

use std::pin::Pin;

use crate::event::{Event, EventData};

/// A trait that defines the behavior of a projection.
/// A projection is a read-model that is built from a stream of events.
pub trait Projection<ED>: Send + Sync
where
    ED: EventData + Send + Sync,
{
    /// Updates the projection with a single event.
    fn apply(
        &mut self,
        event: &Event<ED>,
    ) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send>>;
}
