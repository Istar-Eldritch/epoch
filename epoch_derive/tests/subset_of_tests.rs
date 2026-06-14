//! Integration tests for the `SubsetOf` derive macro (Phase 3).
//!
//! These tests verify end-to-end behaviour of the generated conversion impls:
//! - Round-trip `From`/`TryFrom` assertions
//! - `Saga::EventType` bound satisfaction (compile-time proof via generic function)
//! - All-variants-included smoke test
//! - Generic subset enum support
//! - Compile-fail tests for non-enum inputs and malformed attributes

use epoch_core::event::{EnumConversionError, EventData};
use epoch_derive::{EventData, SubsetOf};
use serde::{Deserialize, Serialize};

// ── Superset enum ─────────────────────────────────────────────────────────────

/// Full application event enum, playing the role of a "foreign" superset.
///
/// `EventData` is required because the generated `TryFrom` wildcard arm calls
/// `EventData::event_type()` to build the `EnumConversionError` message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, EventData)]
pub enum AppEvent {
    /// Struct variant — carries named fields.
    UserCreated { id: String, name: String },
    /// Unit variant — no fields.
    UserDeleted,
    /// Tuple variant — positional fields.
    StatusChanged(u32, bool),
    /// Struct variant not included in any subset below.
    /// Verifies the wildcard arm fires for excluded variants.
    NewSession { user_id: String },
}

// ── Concrete subset enums ─────────────────────────────────────────────────────

/// Subset containing all three field kinds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, EventData, SubsetOf)]
#[subset_of(AppEvent)]
pub enum MixedSubset {
    UserCreated { id: String, name: String },
    UserDeleted,
    StatusChanged(u32, bool),
}

/// Subset covering only struct variants.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, EventData, SubsetOf)]
#[subset_of(AppEvent)]
pub enum UserSubset {
    UserCreated { id: String, name: String },
}

/// Subset covering only the unit variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, EventData, SubsetOf)]
#[subset_of(AppEvent)]
pub enum DeletionSubset {
    UserDeleted,
}

/// Subset covering only the tuple variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, EventData, SubsetOf)]
#[subset_of(AppEvent)]
pub enum StatusSubset {
    StatusChanged(u32, bool),
}

/// All-variants subset: covers every `AppEvent` variant.
/// The wildcard `TryFrom` arm is unreachable; the `#[allow(unreachable_patterns)]`
/// attribute must be present in the generated code so this subset still passes
/// `cargo clippy -- -D warnings`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, EventData, SubsetOf)]
#[subset_of(AppEvent)]
pub enum FullSubset {
    UserCreated { id: String, name: String },
    UserDeleted,
    StatusChanged(u32, bool),
    NewSession { user_id: String },
}

// ── Generic subset enum ───────────────────────────────────────────────────────

/// A generic superset used only for the generics test below.
///
/// Having a generic superset allows the subset to mirror the type parameter,
/// producing `impl<D: Clone + ...> From<GenericSubset<D>> for GenericSuper<D>` which
/// correctly threads the generic through `split_for_impl()`.
///
/// Notes:
/// - `EventData` is implemented manually because the `EventData` derive macro
///   does not currently support generic enums.
/// - Bounds are placed in a `where` clause rather than inline on `D` to avoid
///   ambiguous `D: Deserialize<'de>` conflicts from the `Deserialize` derive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound(deserialize = "D: serde::de::DeserializeOwned"))]
pub enum GenericSuper<D>
where
    D: Clone + Serialize + serde::de::DeserializeOwned + Send,
{
    /// Struct variant with a generic field.
    Created { data: D },
    /// Unit variant.
    Closed,
}

impl<D> EventData for GenericSuper<D>
where
    D: Clone + Serialize + serde::de::DeserializeOwned + Send,
{
    fn event_type(&self) -> &'static str {
        match self {
            GenericSuper::Created { .. } => "Created",
            GenericSuper::Closed => "Closed",
        }
    }
}

/// Generic subset of [`GenericSuper`].
///
/// Verifies that `split_for_impl()` correctly threads the subset's generic
/// type parameter `D` into all three generated `impl` blocks.
///
/// Note: generic superset paths in `#[subset_of(...)]` must use turbofish
/// notation (`GenericSuper::<D>`) so the Rust tokeniser does not misparse
/// `<D>` as a comparison operator chain inside an attribute argument list.
/// `EventData` is implemented manually for the same reason as `GenericSuper`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, SubsetOf)]
#[serde(bound(deserialize = "D: serde::de::DeserializeOwned"))]
#[subset_of(GenericSuper::<D>)]
pub enum GenericSubset<D>
where
    D: Clone + Serialize + serde::de::DeserializeOwned + Send,
{
    /// Maps to `GenericSuper::Created { data: D }`.
    Created { data: D },
}

impl<D> EventData for GenericSubset<D>
where
    D: Clone + Serialize + serde::de::DeserializeOwned + Send,
{
    fn event_type(&self) -> &'static str {
        match self {
            GenericSubset::Created { .. } => "Created",
        }
    }
}

// ── `Saga::EventType` bound assertion ─────────────────────────────────────────

/// Compile-time proof that the generated `TryFrom<&Super>` impl satisfies the
/// `Saga::EventType` bound: `for<'a> TryFrom<&'a ED, Error = EnumConversionError>`.
///
/// If `TryFrom<&AppEvent> for Sub` is not generated, or if the `Error` associated
/// type is wrong, this function will fail to compile.
fn assert_satisfies_saga_event_type_bound<Sub>()
where
    Sub: for<'a> TryFrom<&'a AppEvent, Error = EnumConversionError>,
{
    // No runtime logic needed — the bound check is purely compile-time.
    let _ = std::marker::PhantomData::<Sub>;
}

#[test]
fn mixed_subset_satisfies_saga_event_type_bound() {
    assert_satisfies_saga_event_type_bound::<MixedSubset>();
}

#[test]
fn user_subset_satisfies_saga_event_type_bound() {
    assert_satisfies_saga_event_type_bound::<UserSubset>();
}

#[test]
fn deletion_subset_satisfies_saga_event_type_bound() {
    assert_satisfies_saga_event_type_bound::<DeletionSubset>();
}

// ── Round-trip: From<Sub> for Super ──────────────────────────────────────────

#[test]
fn from_struct_variant_round_trip() {
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
fn from_unit_variant_round_trip() {
    let sub = DeletionSubset::UserDeleted;
    let sup: AppEvent = AppEvent::from(sub);
    assert_eq!(sup, AppEvent::UserDeleted);
}

#[test]
fn from_tuple_variant_round_trip() {
    let sub = StatusSubset::StatusChanged(99, false);
    let sup: AppEvent = AppEvent::from(sub);
    assert_eq!(sup, AppEvent::StatusChanged(99, false));
}

#[test]
fn from_mixed_all_three_kinds() {
    let sub_struct = MixedSubset::UserCreated {
        id: "x".to_string(),
        name: "Y".to_string(),
    };
    let sup: AppEvent = AppEvent::from(sub_struct);
    assert_eq!(
        sup,
        AppEvent::UserCreated {
            id: "x".to_string(),
            name: "Y".to_string(),
        }
    );

    let sub_unit = MixedSubset::UserDeleted;
    let sup: AppEvent = AppEvent::from(sub_unit);
    assert_eq!(sup, AppEvent::UserDeleted);

    let sub_tuple = MixedSubset::StatusChanged(3, true);
    let sup: AppEvent = AppEvent::from(sub_tuple);
    assert_eq!(sup, AppEvent::StatusChanged(3, true));
}

// ── Round-trip: TryFrom<Super> for Sub (owned) ───────────────────────────────

#[test]
fn try_from_owned_included_variant() {
    let sup = AppEvent::UserCreated {
        id: "u2".to_string(),
        name: "Bob".to_string(),
    };
    let sub = UserSubset::try_from(sup).expect("should succeed for included variant");
    assert_eq!(
        sub,
        UserSubset::UserCreated {
            id: "u2".to_string(),
            name: "Bob".to_string(),
        }
    );
}

#[test]
fn try_from_owned_excluded_variant_returns_err() {
    let sup = AppEvent::NewSession {
        user_id: "s1".to_string(),
    };
    let result = UserSubset::try_from(sup);
    assert!(
        result.is_err(),
        "excluded variant should produce Err, not Ok"
    );
    let err = result.unwrap_err();
    // The error message contains the event_type() of the excluded variant.
    assert!(
        format!("{err}").contains("NewSession"),
        "error should mention 'NewSession', got: {err}"
    );
}

#[test]
fn try_from_owned_unit_variant() {
    let sup = AppEvent::UserDeleted;
    let sub = DeletionSubset::try_from(sup).expect("UserDeleted is in DeletionSubset");
    assert_eq!(sub, DeletionSubset::UserDeleted);
}

#[test]
fn try_from_owned_tuple_variant() {
    let sup = AppEvent::StatusChanged(42, true);
    let sub = StatusSubset::try_from(sup).expect("StatusChanged is in StatusSubset");
    assert_eq!(sub, StatusSubset::StatusChanged(42, true));
}

// ── Round-trip: TryFrom<&Super> for Sub (reference) ──────────────────────────

#[test]
fn try_from_ref_included_struct_variant() {
    let sup = AppEvent::UserCreated {
        id: "u3".to_string(),
        name: "Carol".to_string(),
    };
    let sub = UserSubset::try_from(&sup).expect("should succeed for included variant");
    assert_eq!(
        sub,
        UserSubset::UserCreated {
            id: "u3".to_string(),
            name: "Carol".to_string(),
        }
    );
    // Original is not consumed.
    assert_eq!(
        sup,
        AppEvent::UserCreated {
            id: "u3".to_string(),
            name: "Carol".to_string(),
        }
    );
}

#[test]
fn try_from_ref_included_unit_variant() {
    let sup = AppEvent::UserDeleted;
    let sub = DeletionSubset::try_from(&sup).expect("UserDeleted is in DeletionSubset");
    assert_eq!(sub, DeletionSubset::UserDeleted);
}

#[test]
fn try_from_ref_included_tuple_variant() {
    let sup = AppEvent::StatusChanged(7, false);
    let sub = StatusSubset::try_from(&sup).expect("StatusChanged is in StatusSubset");
    assert_eq!(sub, StatusSubset::StatusChanged(7, false));
}

#[test]
fn try_from_ref_excluded_variant_returns_err() {
    let sup = AppEvent::NewSession {
        user_id: "s2".to_string(),
    };
    let result = MixedSubset::try_from(&sup);
    assert!(
        result.is_err(),
        "excluded variant should produce Err, not Ok"
    );
    let err = result.unwrap_err();
    assert!(
        format!("{err}").contains("NewSession"),
        "error should mention 'NewSession', got: {err}"
    );
}

// ── All-variants-included smoke test ─────────────────────────────────────────

/// When every superset variant is in the subset, `TryFrom` must still compile
/// (the wildcard arm is unreachable but the `#[allow(unreachable_patterns)]` attribute
/// prevents the clippy warning from turning into an error).
#[test]
fn full_subset_covers_all_variants() {
    let cases: Vec<AppEvent> = vec![
        AppEvent::UserCreated {
            id: "x".to_string(),
            name: "y".to_string(),
        },
        AppEvent::UserDeleted,
        AppEvent::StatusChanged(1, true),
        AppEvent::NewSession {
            user_id: "z".to_string(),
        },
    ];

    for event in cases {
        let sup_clone = event.clone();
        let sub = FullSubset::try_from(event).expect("FullSubset covers every AppEvent variant");
        // Converting back must produce the original value.
        let restored: AppEvent = AppEvent::from(sub);
        assert_eq!(restored, sup_clone);
    }
}

// ── Generic subset tests ──────────────────────────────────────────────────

/// Verifies the `Saga::EventType` bound is satisfied by the generic subset.
///
/// `GenericSubset<D>` must satisfy `for<'a> TryFrom<&'a GenericSuper<D>, Error = EnumConversionError>`.
fn assert_generic_subset_satisfies_bound<D>()
where
    D: Clone + Serialize + serde::de::DeserializeOwned + Send,
    for<'a> GenericSubset<D>: TryFrom<&'a GenericSuper<D>, Error = EnumConversionError>,
{
    let _ = std::marker::PhantomData::<D>;
}

#[test]
fn generic_subset_satisfies_bound() {
    assert_generic_subset_satisfies_bound::<String>();
}

#[test]
fn generic_subset_from_round_trip() {
    let sub: GenericSubset<String> = GenericSubset::Created {
        data: "hello".to_string(),
    };
    let sup: GenericSuper<String> = GenericSuper::from(sub);
    assert_eq!(
        sup,
        GenericSuper::Created {
            data: "hello".to_string()
        }
    );
}

#[test]
fn generic_subset_try_from_owned_included() {
    let sup: GenericSuper<String> = GenericSuper::Created {
        data: "world".to_string(),
    };
    let sub: GenericSubset<String> =
        GenericSubset::try_from(sup).expect("Created is in GenericSubset");
    assert_eq!(
        sub,
        GenericSubset::Created {
            data: "world".to_string()
        }
    );
}

#[test]
fn generic_subset_try_from_owned_excluded() {
    let sup: GenericSuper<String> = GenericSuper::Closed;
    let result = GenericSubset::<String>::try_from(sup);
    assert!(result.is_err(), "Closed is not in GenericSubset");
    let err = result.unwrap_err();
    assert!(
        format!("{err}").contains("Closed"),
        "error should mention 'Closed', got: {err}"
    );
}

#[test]
fn generic_subset_try_from_ref_included() {
    let sup: GenericSuper<String> = GenericSuper::Created {
        data: "ref_test".to_string(),
    };
    let sub: GenericSubset<String> =
        GenericSubset::try_from(&sup).expect("Created is in GenericSubset");
    assert_eq!(
        sub,
        GenericSubset::Created {
            data: "ref_test".to_string()
        }
    );
    // Original not consumed
    assert_eq!(
        sup,
        GenericSuper::Created {
            data: "ref_test".to_string()
        }
    );
}

// ── Compile-fail tests ────────────────────────────────────────────────────────

/// Runs compile-fail snapshots via `trybuild`.
///
/// Each `.rs` file in `tests/compile-fail/subset_of/` is compiled; the test
/// passes when every file produces a compile error whose output matches the
/// adjacent `.stderr` snapshot.
#[test]
fn compile_fail_tests() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile-fail/subset_of/*.rs");
}
