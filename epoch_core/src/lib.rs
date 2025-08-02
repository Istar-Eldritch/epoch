//! # Epoch

#![deny(missing_docs)]

pub mod aggregate;
pub mod event;
pub mod event_store;
pub mod projection;
pub mod saga;
pub mod state_store;

/// Re-exports the most commonly used traits and types for convenience.
pub mod prelude {
    pub use super::aggregate::*;
    pub use super::event::*;
    pub use super::event_store::*;
    pub use super::projection::*;
    pub use super::saga::*;
    pub use super::state_store::*;
}
