//! Consumer-side subset enum derive example.
//!
//! This example demonstrates `#[derive(SubsetOf)]`, the consumer-side
//! counterpart to the producer-side `#[subset_enum]` attribute macro.
//!
//! The key difference:
//! - `#[subset_enum]` is placed **on the superset enum** (producer owns it).
//! - `#[derive(SubsetOf)]` is placed **on the consumer's enum** (the superset
//!   can live in a crate you don't own).
//!
//! The macro generates three conversion impls:
//! - `From<Sub> for Super` — total, moves ownership
//! - `TryFrom<Super> for Sub` — owned narrowing
//! - `TryFrom<&Super> for Sub` — reference narrowing with per-field `.clone()`
//!
//! # Import note
//!
//! The `EventData` *trait* (type namespace) and the `EventData` *derive macro*
//! (macro namespace) live in separate Rust namespaces and can both be in scope
//! simultaneously without conflict.

// The `EventData` trait from epoch_core (type namespace).
use epoch_core::event::EventData;
// The `EventData` derive macro and `SubsetOf` derive macro (macro namespace).
use epoch_derive::{EventData, SubsetOf};
use serde::{Deserialize, Serialize};

// ── Superset (simulates a crate you don't control) ────────────────────────────

// In a real project, `AppEvent` would come from `extern crate some_upstream`.
// Here we define it inline so the example is self-contained.

/// Full application event enum — the "upstream" superset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, EventData)]
pub enum AppEvent {
    UserCreated { id: String, name: String },
    UserDeleted,
    StatusChanged(u32, bool),
    NewSession { user_id: String },
}

// ── Consumer-side subset enums ────────────────────────────────────────────────

/// A saga that only cares about user lifecycle events.
///
/// `#[derive(SubsetOf)]` generates:
/// - `From<UserSagaEvent> for AppEvent`
/// - `TryFrom<AppEvent> for UserSagaEvent`
/// - `TryFrom<&AppEvent> for UserSagaEvent`
///
/// If any declared variant name or field name doesn't match `AppEvent`,
/// the compiler reports a type error — structural compatibility is enforced
/// at compile time, not at runtime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, EventData, SubsetOf)]
#[subset_of(AppEvent)]
pub enum UserSagaEvent {
    UserCreated { id: String, name: String },
    UserDeleted,
}

/// Another consumer that only tracks status changes (tuple variant example).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, EventData, SubsetOf)]
#[subset_of(AppEvent)]
pub enum StatusMonitorEvent {
    StatusChanged(u32, bool),
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn main() {
    // 1. From<Sub> for Super — total conversion, moves ownership.
    let sub = UserSagaEvent::UserCreated {
        id: "u1".to_string(),
        name: "Alice".to_string(),
    };
    let sup: AppEvent = AppEvent::from(sub);
    println!("From (owned):    {sup:?}");
    assert_eq!(
        sup,
        AppEvent::UserCreated {
            id: "u1".to_string(),
            name: "Alice".to_string(),
        }
    );

    // 2. TryFrom<Super> for Sub — narrowing, returns Err for excluded variants.
    let sup_included = AppEvent::UserDeleted;
    let sub: UserSagaEvent = UserSagaEvent::try_from(sup_included).expect("included variant");
    println!("TryFrom (owned, included): {sub:?}");
    assert_eq!(sub, UserSagaEvent::UserDeleted);

    let sup_excluded = AppEvent::NewSession {
        user_id: "s1".to_string(),
    };
    let err = UserSagaEvent::try_from(sup_excluded).unwrap_err();
    println!("TryFrom (owned, excluded): {err}");

    // 3. TryFrom<&Super> for Sub — reference narrowing, clones matched fields.
    //    This is the impl that satisfies the `Saga::EventType` bound:
    //    `for<'a> TryFrom<&'a AppEvent, Error = EnumConversionError>`.
    let sup_ref = AppEvent::UserCreated {
        id: "u2".to_string(),
        name: "Bob".to_string(),
    };
    let sub: UserSagaEvent = UserSagaEvent::try_from(&sup_ref).expect("included variant");
    println!("TryFrom (ref,   included): {sub:?}");
    // Original is still accessible — not moved.
    println!("Original still alive:      {sup_ref:?}");
    assert_eq!(
        sub,
        UserSagaEvent::UserCreated {
            id: "u2".to_string(),
            name: "Bob".to_string(),
        }
    );

    // 4. Tuple variant example using StatusMonitorEvent.
    let sup_status = AppEvent::StatusChanged(99, true);
    let mon: StatusMonitorEvent =
        StatusMonitorEvent::try_from(&sup_status).expect("StatusChanged is in StatusMonitorEvent");
    println!("StatusMonitorEvent:        {mon:?}");
    assert_eq!(mon, StatusMonitorEvent::StatusChanged(99, true));

    // 5. Excluded variant on the StatusMonitorEvent subset.
    let sup_user = AppEvent::UserDeleted;
    let err = StatusMonitorEvent::try_from(&sup_user).unwrap_err();
    println!("StatusMonitor excluded:    {err}");
    assert!(format!("{err}").contains("UserDeleted"));

    println!("\nAll assertions passed ✓");
}
