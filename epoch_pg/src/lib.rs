//! # Epoch postgres store

#![deny(missing_docs)]

/// The event_store module exports implementations of the EventStoreBackend for postgres
pub mod event_store;

pub use event_store::*;
