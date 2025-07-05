//! # Epoch memory store

#![deny(missing_docs)]

mod event_store;
mod projection_store;

pub use event_store::*;
pub use projection_store::*;
