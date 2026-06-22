//! Schema evolution example: forward-compatible event upcasting.
//!
//! Event sourcing stores facts forever. When a persisted event type's schema
//! changes — a renamed field, a new required field, a type change — stored v1
//! JSON can no longer deserialize into the updated Rust type. Upcasting solves
//! this by transforming stored JSON one version at a time before deserialization.
//!
//! Scenario modelled here:
//!   - v1: `OrderPlaced { product: String }` (original schema)
//!   - v2: `OrderPlaced { product: String, currency: String }` (new required field)
//!   - Upcaster v1→v2 supplies `"USD"` as the default currency for old records.
//!
//! Run with:
//!   `cargo run --example schema-evolution --features upcasting`

use epoch::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

// ── Current event schema (v2) ─────────────────────────────────────────────────

/// The current version of the event. All new writes produce schema_version=2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderPlaced {
    pub product: String,
    pub currency: String,
}

impl EventData for OrderPlaced {
    fn event_type(&self) -> &'static str {
        "OrderPlaced"
    }
    fn schema_version(&self) -> SchemaVersion {
        2
    }
}

// ── Upcaster: v1 → v2 ────────────────────────────────────────────────────────

/// Transforms a v1 `OrderPlaced` payload (no `currency` key) into v2 by
/// supplying `"USD"` as the default currency.
pub struct OrderPlacedV1ToV2;

impl Upcaster for OrderPlacedV1ToV2 {
    fn event_type(&self) -> &str {
        "OrderPlaced"
    }

    fn from_version(&self) -> SchemaVersion {
        1
    }

    fn upcast(&self, _ctx: &UpcastContext<'_>, mut payload: Value) -> Result<Value, UpcastError> {
        if let Value::Object(map) = &mut payload {
            // Only insert the default; if a v1 row somehow already has `currency`
            // (shouldn't happen, but idempotency is good practice), leave it alone.
            map.entry("currency").or_insert_with(|| json!("USD"));
        }
        Ok(payload)
    }
}

// ── Dead-letter sink ──────────────────────────────────────────────────────────

/// Collects dead-lettered events in memory for inspection.
pub struct InMemoryDeadLetterSink {
    pub events: Arc<Mutex<Vec<DeadLetteredEvent>>>,
}

impl InMemoryDeadLetterSink {
    fn new() -> (Self, Arc<Mutex<Vec<DeadLetteredEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (Self { events: Arc::clone(&events) }, events)
    }
}

#[async_trait::async_trait]
impl DeadLetterSink for InMemoryDeadLetterSink {
    async fn capture(&self, event: DeadLetteredEvent) {
        println!(
            "[DEAD LETTER] id={} type={} stored_v={} reason={}",
            event.event_id, event.event_type, event.stored_version, event.reason
        );
        self.events.lock().unwrap().push(event);
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let stream_id = Uuid::new_v4();
    let event_id = Uuid::new_v4();

    // ── Part 1: upcasting a v1 payload to v2 ─────────────────────────────────

    let mut registry = UpcasterRegistry::new();
    registry.register(OrderPlacedV1ToV2);

    println!("--- Part 1: upcast v1 → v2 ---");

    // This simulates a row read from storage: schema_version=1, no `currency`.
    let v1_payload = json!({ "product": "Widget" });

    let order: OrderPlaced = registry
        .upcast_and_deserialize("OrderPlaced", 1, stream_id, event_id, Some(v1_payload))
        .await?
        .expect("v1 payload should upcast successfully");

    println!("  Upcast result: {order:?}");
    assert_eq!(order.product, "Widget");
    assert_eq!(order.currency, "USD", "upcaster should supply default currency");

    // ── Part 2: v2 payload passes through unchanged ───────────────────────────

    println!("\n--- Part 2: v2 payload passes through ---");

    let v2_payload = json!({ "product": "Gadget", "currency": "EUR" });

    let order: OrderPlaced = registry
        .upcast_and_deserialize("OrderPlaced", 2, stream_id, event_id, Some(v2_payload))
        .await?
        .expect("v2 payload needs no upcasting");

    println!("  Direct v2: {order:?}");
    assert_eq!(order.currency, "EUR");

    // ── Part 3: multi-step chain (v1 → v2 → v3) ──────────────────────────────
    //
    // When you add a third field later, just register another upcaster for 2→3.
    // The registry chains steps automatically.

    println!("\n--- Part 3: chained upcasters (v1 → v2 → v3) ---");

    // Hypothetical v3 adds a `quantity` field.
    struct OrderPlacedV2ToV3;

    impl Upcaster for OrderPlacedV2ToV3 {
        fn event_type(&self) -> &str {
            "OrderPlaced"
        }
        fn from_version(&self) -> SchemaVersion {
            2
        }
        fn upcast(
            &self,
            _ctx: &UpcastContext<'_>,
            mut payload: Value,
        ) -> Result<Value, UpcastError> {
            if let Value::Object(map) = &mut payload {
                map.entry("quantity").or_insert_with(|| json!(1));
            }
            Ok(payload)
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct OrderPlacedV3 {
        product: String,
        currency: String,
        quantity: u32,
    }

    impl EventData for OrderPlacedV3 {
        fn event_type(&self) -> &'static str {
            "OrderPlaced"
        }
        fn schema_version(&self) -> SchemaVersion {
            3
        }
    }

    let mut chain_registry = UpcasterRegistry::new();
    chain_registry
        .register(OrderPlacedV1ToV2)
        .register(OrderPlacedV2ToV3);

    assert_eq!(chain_registry.current_version("OrderPlaced"), 3);

    let v1_payload = json!({ "product": "Widget" });

    let order: OrderPlacedV3 = chain_registry
        .upcast_and_deserialize("OrderPlaced", 1, stream_id, event_id, Some(v1_payload))
        .await?
        .expect("chain should take v1 all the way to v3");

    println!("  Chain result: {order:?}");
    assert_eq!(order.currency, "USD");
    assert_eq!(order.quantity, 1);
    assert_eq!(chain_registry.counters().applied(), 2, "two upcaster steps applied");

    // ── Part 4: FailurePolicy::DeadLetter ────────────────────────────────────
    //
    // Under the default Fail policy, an unrecoverable event aborts the stream.
    // DeadLetter policy routes it to a sink and continues — the stream keeps
    // flowing and the bad event can be inspected and recovered later.

    println!("\n--- Part 4: DeadLetter policy ---");

    let (sink, captured) = InMemoryDeadLetterSink::new();
    let mut lenient = UpcasterRegistry::new();
    lenient
        .register(OrderPlacedV1ToV2)
        .with_policy(FailurePolicy::DeadLetter)
        .with_dead_letter_sink(sink);

    // This payload has the wrong type for `product` — it will fail to deserialize
    // even after upcasting, triggering the dead-letter path.
    let corrupt_payload = json!({ "product": 12345 });

    let result: Result<Option<OrderPlaced>, _> = lenient
        .upcast_and_deserialize("OrderPlaced", 1, stream_id, event_id, Some(corrupt_payload))
        .await;

    // Under DeadLetter, the stream continues (Ok(None)) instead of erroring out.
    assert!(
        matches!(result, Ok(None)),
        "expected Ok(None) under DeadLetter policy, got {result:?}"
    );
    assert_eq!(lenient.counters().dead_lettered(), 1);
    println!(
        "  Dead-lettered events captured: {}",
        captured.lock().unwrap().len()
    );

    println!("\nAll assertions passed!");
    Ok(())
}
