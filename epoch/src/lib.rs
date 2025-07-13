//! # Epoch

#![deny(missing_docs)]

#[cfg(feature = "derive")]
/// Proc-macros for the `epoch` crate.
pub mod derive {
    //! Contains proc-macros for the `epoch` crate.
    pub use epoch_derive::*;
}

#[cfg(feature = "in-memory")]
/// Proc-macros for the `epoch` crate.
pub mod mem_store {
    //! Contains proc-macros for the `epoch` crate.
    pub use epoch_mem::*;
}

#[cfg(feature = "postgres")]
/// Proc-macros for the `epoch` crate.
pub mod pg_store {
    //! Contains proc-macros for the `epoch` crate.
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
