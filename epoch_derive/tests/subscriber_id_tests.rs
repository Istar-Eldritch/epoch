//! Tests for the SubscriberId derive macro.

use epoch_core::SubscriberId;
use epoch_derive::SubscriberId;

/// Test basic derivation with automatic name generation.
#[derive(SubscriberId)]
struct UserProfileProjection;

#[test]
fn basic_derivation_strips_projection_suffix() {
    let p = UserProfileProjection;
    assert_eq!(p.subscriber_id(), "projection:user-profile");
}

/// Test derivation without common suffix.
#[derive(SubscriberId)]
struct OrderSummary;

#[test]
fn basic_derivation_without_suffix() {
    let p = OrderSummary;
    assert_eq!(p.subscriber_id(), "projection:order-summary");
}

/// Test derivation with Saga suffix.
#[derive(SubscriberId)]
struct OrderProcessSaga;

#[test]
fn strips_saga_suffix() {
    let p = OrderProcessSaga;
    assert_eq!(p.subscriber_id(), "projection:order-process");
}

/// Test derivation with Handler suffix.
#[derive(SubscriberId)]
struct EventHandler;

#[test]
fn strips_handler_suffix() {
    let p = EventHandler;
    assert_eq!(p.subscriber_id(), "projection:event");
}

/// Test derivation with Observer suffix.
#[derive(SubscriberId)]
struct StateObserver;

#[test]
fn strips_observer_suffix() {
    let p = StateObserver;
    assert_eq!(p.subscriber_id(), "projection:state");
}

/// Test derivation with ProcessManager suffix.
#[derive(SubscriberId)]
struct OrderFulfillmentProcessManager;

#[test]
fn strips_process_manager_suffix() {
    let p = OrderFulfillmentProcessManager;
    assert_eq!(p.subscriber_id(), "projection:order-fulfillment");
}

/// Test custom subscriber ID.
#[derive(SubscriberId)]
#[subscriber_id("my-custom-id")]
struct CustomIdProjection;

#[test]
fn custom_id_attribute() {
    let p = CustomIdProjection;
    assert_eq!(p.subscriber_id(), "projection:my-custom-id");
}

/// Test custom prefix.
#[derive(SubscriberId)]
#[subscriber_id(prefix = "saga")]
struct MySagaWithPrefix;

#[test]
fn custom_prefix() {
    let p = MySagaWithPrefix;
    assert_eq!(p.subscriber_id(), "saga:my-saga-with-prefix");
}

/// Test custom prefix with saga suffix stripped.
#[derive(SubscriberId)]
#[subscriber_id(prefix = "saga")]
struct PaymentProcessSaga;

#[test]
fn custom_prefix_with_suffix_stripped() {
    let p = PaymentProcessSaga;
    assert_eq!(p.subscriber_id(), "saga:payment-process");
}

/// Test custom name and prefix.
#[derive(SubscriberId)]
#[subscriber_id(name = "inventory", prefix = "handler")]
struct InventoryUpdateHandler;

#[test]
fn custom_name_and_prefix() {
    let p = InventoryUpdateHandler;
    assert_eq!(p.subscriber_id(), "handler:inventory");
}

/// Test acronyms in names.
#[derive(SubscriberId)]
struct HTTPRequestHandler;

#[test]
fn handles_acronyms() {
    let p = HTTPRequestHandler;
    // Handler suffix stripped, HTTP stays as acronym
    assert_eq!(p.subscriber_id(), "projection:http-request");
}

/// Test struct with fields.
#[derive(SubscriberId)]
struct ProjectionWithFields {
    #[allow(dead_code)]
    some_field: String,
    #[allow(dead_code)]
    another_field: i32,
}

#[test]
fn works_with_fields() {
    let p = ProjectionWithFields {
        some_field: "test".to_string(),
        another_field: 42,
    };
    assert_eq!(p.subscriber_id(), "projection:projection-with-fields");
}

/// Test single word name.
#[derive(SubscriberId)]
struct Counter;

#[test]
fn single_word_name() {
    let p = Counter;
    assert_eq!(p.subscriber_id(), "projection:counter");
}

/// Test struct with generic parameters (should still work).
#[derive(SubscriberId)]
struct GenericProjection<T> {
    #[allow(dead_code)]
    _marker: std::marker::PhantomData<T>,
}

#[test]
fn works_with_generics() {
    let p: GenericProjection<String> = GenericProjection {
        _marker: std::marker::PhantomData,
    };
    assert_eq!(p.subscriber_id(), "projection:generic");
}
