//! Tests for the `#[event_data(schema_version = N)]` attribute on `#[derive(EventData)]`.
//!
//! Verifies that:
//! - Absence of the attribute yields the default `schema_version()` of `1`.
//! - Presence of `#[event_data(schema_version = N)]` overrides `schema_version()` to `N`.
//! - The `event_type()` method is unaffected.

use epoch_core::event::EventData;
use epoch_derive::EventData;
use serde::{Deserialize, Serialize};

// ── Enum without schema_version attribute (should default to 1) ───────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, EventData)]
pub enum NoVersionEvent {
    OrderPlaced { amount: u64 },
    OrderCancelled,
}

// ── Enum with schema_version = 2 ─────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, EventData)]
#[event_data(schema_version = 2)]
pub enum V2Event {
    OrderPlaced { amount: u64, currency: String },
    OrderCancelled,
}

// ── Enum with schema_version = 5 ─────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, EventData)]
#[event_data(schema_version = 5)]
pub enum V5Event {
    UserMigrated { old_id: String, new_id: String },
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[test]
fn no_attribute_defaults_to_version_one() {
    let event = NoVersionEvent::OrderPlaced { amount: 100 };
    assert_eq!(event.schema_version(), 1);
    assert_eq!(event.event_type(), "OrderPlaced");
}

#[test]
fn schema_version_attribute_two_is_returned() {
    let placed = V2Event::OrderPlaced {
        amount: 99,
        currency: "USD".to_string(),
    };
    assert_eq!(placed.schema_version(), 2);
    assert_eq!(placed.event_type(), "OrderPlaced");

    let cancelled = V2Event::OrderCancelled;
    // schema_version is the same for every variant of the enum.
    assert_eq!(cancelled.schema_version(), 2);
    assert_eq!(cancelled.event_type(), "OrderCancelled");
}

#[test]
fn schema_version_attribute_five_is_returned() {
    let event = V5Event::UserMigrated {
        old_id: "a".to_string(),
        new_id: "b".to_string(),
    };
    assert_eq!(event.schema_version(), 5);
    assert_eq!(event.event_type(), "UserMigrated");
}
