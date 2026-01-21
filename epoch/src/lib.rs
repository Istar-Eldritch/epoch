//! # Epoch

#![deny(missing_docs)]

/// Proc-macros for the `epoch` crate.
#[cfg(feature = "derive")]
pub mod derive {
    pub use epoch_derive::*;
}

/// In-memory event store implementation.
#[cfg(feature = "in-memory")]
pub mod mem_store {
    pub use epoch_mem::*;
}

/// PostgreSQL event store implementation.
#[cfg(feature = "postgres")]
pub mod pg_store {
    pub use epoch_pg::*;
}

pub mod prelude {
    //! The prelude module for the `epoch` crate.
    pub use epoch_core::prelude::*;

    #[cfg(feature = "derive")]
    pub use super::derive::*;
    #[cfg(feature = "in-memory")]
    pub use super::mem_store::*;
    #[cfg(feature = "postgres")]
    pub use super::pg_store::*;
}
