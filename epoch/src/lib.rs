//! # Epoch

#![deny(missing_docs)]

#[cfg(feature = "derive")]
/// Proc-macros for the `epoch` crate.
pub mod derive {
    //! Contains proc-macros for the `epoch` crate.
    pub use epoch_derive::subset_enum;
}

pub mod prelude {
    //! The prelude module for the `epoch` crate.
    pub use epoch_core::prelude::*;

    #[cfg(feature = "derive")]
    pub use super::derive::*;
}
