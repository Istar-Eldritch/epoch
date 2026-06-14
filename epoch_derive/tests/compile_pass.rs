//! Compile-pass integration tests for the `SubsetOf` derive macro (Phase 1).
//!
//! These tests prove that the generated `From<Sub> for Super` body compiles
//! and behaves correctly against a real superset enum defined in the same crate,
//! covering all three field kinds: struct variants, unit variants, and tuple variants.

use epoch_derive::SubsetOf;

/// Concrete superset enum used throughout these tests.
#[derive(Debug, Clone, PartialEq)]
pub enum AppEvent {
    /// Struct variant.
    UserCreated { id: String, name: String },
    /// Unit variant.
    UserDeleted,
    /// Tuple variant.
    StatusChanged(u32, bool),
    /// Struct variant not included in any subset below — verifies the macro
    /// does not require the subset to cover every superset variant.
    NewSession { user_id: String },
}

// ── Subset enums ──────────────────────────────────────────────────────────────

/// Subset containing only a struct variant.
#[derive(Debug, Clone, PartialEq, SubsetOf)]
#[subset_of(AppEvent)]
enum UserSubset {
    UserCreated { id: String, name: String },
}

/// Subset containing only a unit variant.
#[derive(Debug, Clone, PartialEq, SubsetOf)]
#[subset_of(AppEvent)]
enum DeletionSubset {
    UserDeleted,
}

/// Subset containing only a tuple variant.
#[derive(Debug, Clone, PartialEq, SubsetOf)]
#[subset_of(AppEvent)]
enum StatusSubset {
    StatusChanged(u32, bool),
}

/// Subset containing all three field kinds at once.
#[derive(Debug, Clone, PartialEq, SubsetOf)]
#[subset_of(AppEvent)]
enum MixedSubset {
    UserCreated { id: String, name: String },
    UserDeleted,
    StatusChanged(u32, bool),
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[test]
fn from_struct_variant() {
    let sub = UserSubset::UserCreated {
        id: "u1".to_string(),
        name: "Alice".to_string(),
    };
    let sup: AppEvent = AppEvent::from(sub);
    assert_eq!(
        sup,
        AppEvent::UserCreated {
            id: "u1".to_string(),
            name: "Alice".to_string(),
        }
    );
}

#[test]
fn from_unit_variant() {
    let sub = DeletionSubset::UserDeleted;
    let sup: AppEvent = AppEvent::from(sub);
    assert_eq!(sup, AppEvent::UserDeleted);
}

#[test]
fn from_tuple_variant() {
    let sub = StatusSubset::StatusChanged(42, true);
    let sup: AppEvent = AppEvent::from(sub);
    assert_eq!(sup, AppEvent::StatusChanged(42, true));
}

#[test]
fn from_mixed_struct() {
    let sub = MixedSubset::UserCreated {
        id: "u2".to_string(),
        name: "Bob".to_string(),
    };
    let sup: AppEvent = AppEvent::from(sub);
    assert_eq!(
        sup,
        AppEvent::UserCreated {
            id: "u2".to_string(),
            name: "Bob".to_string(),
        }
    );
}

#[test]
fn from_mixed_unit() {
    let sub = MixedSubset::UserDeleted;
    let sup: AppEvent = AppEvent::from(sub);
    assert_eq!(sup, AppEvent::UserDeleted);
}

#[test]
fn from_mixed_tuple() {
    let sub = MixedSubset::StatusChanged(7, false);
    let sup: AppEvent = AppEvent::from(sub);
    assert_eq!(sup, AppEvent::StatusChanged(7, false));
}
