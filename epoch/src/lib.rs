//! # Epoch

#![deny(missing_docs)]

#[cfg(feature = "derive")]
/// Proc-macros for the `epoch` crate.
pub mod derive {
    //! Contains proc-macros for the `epoch` crate.
    pub use epoch_derive::*;
}

#[cfg(feature = "mem_store")]
/// Proc-macros for the `epoch` crate.
pub mod mem_store {
    //! Contains proc-macros for the `epoch` crate.
    pub use epoch_mem_store::*;
}

pub mod prelude {
    //! The prelude module for the `epoch` crate.
    pub use epoch_core::prelude::*;

    #[cfg(feature = "derive")]
    pub use super::derive::*;
    #[cfg(feature = "mem_store")]
    pub use super::mem_store::*;
}
