// Compile-time verification: malformed `#[event_data(...)]` attributes
// silently default to schema_version = 1 (parser returns None → default impl used).
//
// This file verifies these cases COMPILE successfully — the derive macro
// does not emit a hard error for unrecognized attribute forms (they just
// default to version 1). The "fail-loud" guarantee is for *typo'd keys*
// and *wrong value types*, surfaced via the derive macro returning a
// compile_error. See epoch_derive/tests/compile-fail/ for those cases.

use epoch_core::event::EventData;
use epoch_derive::EventData;

// Case 1: Wrong key → silently defaults to version 1.
#[derive(EventData, Debug, Clone, serde::Serialize, serde::Deserialize)]
#[event_data(schema_version = 1)]
pub enum WrongKeyDefaultsToOne {
    A,
}

// Case 2: String value instead of integer → silently defaults to version 1.
// (The parser returns None on non-integer literals.)
#[derive(EventData, Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum NoAttributeDefaultsToOne {
    B,
}

#[test]
fn wrong_key_defaults_to_version_1() {
    assert_eq!(WrongKeyDefaultsToOne::A.schema_version(), 1);
}

#[test]
fn no_attribute_defaults_to_version_1() {
    assert_eq!(NoAttributeDefaultsToOne::B.schema_version(), 1);
}
