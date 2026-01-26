//! # Epoch
//!
//! Epoch is a Rust framework for building event-sourced systems using CQRS
//! (Command Query Responsibility Segregation) and event sourcing patterns.
//!
//! ## Trait Hierarchy
//!
//! Epoch uses a clear trait hierarchy to separate concerns:
//!
//! - [`EventApplicator<ED>`](event_applicator::EventApplicator) - Base trait for applying events to state
//!   - [`Projection<ED>`](projection::Projection) - Read models that subscribe to the event bus
//!   - [`Aggregate<ED>`](aggregate::Aggregate) - Command handlers that produce events
//!
//! ### Why Aggregates Can't Be Projections
//!
//! Aggregates persist their state in `handle()` before publishing events. Subscribing
//! them to the event bus would cause duplicate writes and race conditions. The type
//! system enforces this: [`ProjectionHandler`](projection::ProjectionHandler) only
//! accepts [`Projection`](projection::Projection), not [`Aggregate`](aggregate::Aggregate).

#![deny(missing_docs)]

pub mod aggregate;
pub mod event;
pub mod event_applicator;
pub mod event_store;
pub mod projection;
pub mod saga;
pub mod state_store;

/// Re-exports the most commonly used traits and types for convenience.
pub mod prelude {
    pub use super::aggregate::*;
    pub use super::event::*;
    pub use super::event_applicator::*;
    pub use super::event_store::*;
    pub use super::projection::*;
    pub use super::saga::*;
    pub use super::state_store::*;
}
