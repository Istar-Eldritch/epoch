//! # Epoch

#![deny(missing_docs)]

mod event;
mod event_store;
mod projection;

pub use event::*;
pub use event_store::*;
pub use projection::*;

pub mod prelude {
    //! The prelude module for the `epoch` crate.
    pub use super::{
        EnumConversionError, Event, EventBuilder, EventBuilderError, EventBus, EventData,
        EventStoreBackend, EventStream, Projection,
    };
}

/// A trait that defines the behavior of an event bus.
#[async_trait::async_trait]
pub trait EventBus {
    /// The type of event that can be published to this event bus.
    type EventType: EventData;
    /// The error returned by publish
    type PublishError: std::error::Error;
    /// Publishes events to the event bus.
    async fn publish(&self, event: Event<Self::EventType>) -> Result<(), Self::PublishError>;

    /// Allows to subscribe to events
    fn subscribe(&self, projector: impl Projector) -> ();
}
