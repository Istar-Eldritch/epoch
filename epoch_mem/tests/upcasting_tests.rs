//! Registry-direct upcasting parity tests for `epoch_mem`.
//!
//! `epoch_mem` stores typed [`Event<D>`] values in memory and performs no
//! serde round-trip on read, so **upcasting is not needed at the read path**
//! for the in-memory backend. These tests instead exercise the
//! [`UpcasterRegistry`] directly (JSON in → upcast → deserialize → `D`) to
//! prove that the mechanism is backend-independent and works correctly
//! regardless of whether a real DB is involved.
//!
//! They also run the portable [`epoch_core::testing::verify_upcasting_chain`]
//! contract helper to satisfy **R-20** and anchor the failure-semantics
//! assertions required by **R-21**.
//!
//! # Features required
//!
//! These tests require `epoch_core` with both `testing` and `upcasting`
//! features enabled. See `epoch_mem/Cargo.toml` `[dev-dependencies]`.

use epoch_core::event::EventData;
use epoch_core::upcasting::{
    DeadLetterSink, DeadLetteredEvent, FailurePolicy, UpcastContext, UpcastError, Upcaster,
    UpcasterRegistry,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

// ── Domain types for the multi-step chain test ────────────────────────────────

/// The current (v3) shape of the `ItemAdded` event, used for testing a
/// two-step chain: v1 → v2 → v3.
///
/// v1 had only `sku`.
/// v2 added `quantity` (upcaster default: 1).
/// v3 added `warehouse` (upcaster default: "main").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ItemAdded {
    sku: String,
    quantity: u32,
    warehouse: String,
}

impl EventData for ItemAdded {
    fn event_type(&self) -> &'static str {
        "ItemAdded"
    }
    fn schema_version(&self) -> epoch_core::event::SchemaVersion {
        3
    }
}

// ── Upcaster helpers ──────────────────────────────────────────────────────────

/// Adds a fixed JSON field to the payload.
struct InjectFieldUpcaster {
    event_type: &'static str,
    from_version: u32,
    field: &'static str,
    value: Value,
}

impl Upcaster for InjectFieldUpcaster {
    fn event_type(&self) -> &str {
        self.event_type
    }
    fn from_version(&self) -> u32 {
        self.from_version
    }
    fn upcast(&self, _ctx: &UpcastContext<'_>, mut payload: Value) -> Result<Value, UpcastError> {
        if let Value::Object(map) = &mut payload {
            map.insert(self.field.to_string(), self.value.clone());
        }
        Ok(payload)
    }
}

// ── In-memory dead-letter sink for assertions ─────────────────────────────────

struct RecordingSink {
    captured: Arc<Mutex<Vec<DeadLetteredEvent>>>,
}

impl RecordingSink {
    fn new() -> (Self, Arc<Mutex<Vec<DeadLetteredEvent>>>) {
        let captured = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                captured: Arc::clone(&captured),
            },
            captured,
        )
    }
}

#[async_trait::async_trait]
impl DeadLetterSink for RecordingSink {
    async fn capture(&self, event: DeadLetteredEvent) {
        self.captured.lock().unwrap().push(event);
    }
}

// ── verify_upcasting_chain contract test (R-20) ───────────────────────────────

/// Runs the portable `verify_upcasting_chain` helper against a two-step chain
/// (v1 → v2 → v3) to verify that the registry correctly sequences the steps
/// and deserializes the result into the current domain type.
#[tokio::test]
async fn upcasting_chain_contract_passes_with_multi_step_chain() {
    let mut registry = UpcasterRegistry::new();
    // Step 1→2: add `quantity` with a default of 1.
    registry.register(InjectFieldUpcaster {
        event_type: "ItemAdded",
        from_version: 1,
        field: "quantity",
        value: json!(1u32),
    });
    // Step 2→3: add `warehouse` with a default of "main".
    registry.register(InjectFieldUpcaster {
        event_type: "ItemAdded",
        from_version: 2,
        field: "warehouse",
        value: json!("main"),
    });
    assert_eq!(registry.current_version("ItemAdded"), 3);

    let v1_payload = json!({ "sku": "WIDGET-42" });
    let expected = ItemAdded {
        sku: "WIDGET-42".to_string(),
        quantity: 1,
        warehouse: "main".to_string(),
    };

    epoch_core::testing::verify_upcasting_chain::<ItemAdded>(
        &registry,
        "ItemAdded",
        v1_payload,
        expected,
    )
    .await;
}

// ── Single-step chain test ────────────────────────────────────────────────────

/// A single registered step (v1 → v2) is enough for `verify_upcasting_chain`.
#[tokio::test]
async fn upcasting_chain_contract_passes_with_single_step_chain() {
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct PriceUpdated {
        amount: u64,
        currency: String,
    }
    impl EventData for PriceUpdated {
        fn event_type(&self) -> &'static str {
            "PriceUpdated"
        }
        fn schema_version(&self) -> u32 {
            2
        }
    }

    let mut registry = UpcasterRegistry::new();
    registry.register(InjectFieldUpcaster {
        event_type: "PriceUpdated",
        from_version: 1,
        field: "currency",
        value: json!("USD"),
    });

    epoch_core::testing::verify_upcasting_chain::<PriceUpdated>(
        &registry,
        "PriceUpdated",
        json!({ "amount": 999 }),
        PriceUpdated {
            amount: 999,
            currency: "USD".to_string(),
        },
    )
    .await;
}

// ── Empty registry (no steps) is a no-op ─────────────────────────────────────

/// An empty registry applies no steps: a payload that already matches the
/// current type deserializes directly without any transformation.
#[tokio::test]
async fn empty_registry_passes_through_current_version_payload() {
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct TicketClosed {
        id: String,
    }
    impl EventData for TicketClosed {
        fn event_type(&self) -> &'static str {
            "TicketClosed"
        }
    }

    let registry = UpcasterRegistry::new();

    // No upcasters → current version is 1 → the payload must already be the v1 shape.
    let result: Result<Option<TicketClosed>, UpcastError> = registry.upcast_and_deserialize(
        "TicketClosed",
        1,
        Uuid::new_v4(),
        Uuid::new_v4(),
        Some(json!({ "id": "abc" })),
    );
    assert_eq!(
        result.unwrap(),
        Some(TicketClosed {
            id: "abc".to_string()
        })
    );
    assert_eq!(registry.counters().succeeded(), 1);
    assert_eq!(registry.counters().applied(), 0, "no steps applied");
}

// ── R-21: failure-semantics parity tests ─────────────────────────────────────

/// **R-3**: a missing step in the chain is detected and surfaced as
/// `UpcastError::MissingStep`, never silently dropped.
#[tokio::test]
async fn missing_step_returns_missing_step_error() {
    let mut registry = UpcasterRegistry::new();
    // Register 1→2 and 3→4 but skip 2→3.
    registry.register(InjectFieldUpcaster {
        event_type: "ItemAdded",
        from_version: 1,
        field: "a",
        value: json!(1),
    });
    registry.register(InjectFieldUpcaster {
        event_type: "ItemAdded",
        from_version: 3,
        field: "c",
        value: json!(3),
    });
    assert_eq!(registry.current_version("ItemAdded"), 4);

    let result: Result<Option<ItemAdded>, UpcastError> = registry.upcast_and_deserialize(
        "ItemAdded",
        1,
        Uuid::new_v4(),
        Uuid::new_v4(),
        Some(json!({ "sku": "X", "quantity": 1, "warehouse": "main" })),
    );
    assert!(
        matches!(
            result,
            Err(UpcastError::MissingStep {
                from_version: 2,
                ..
            })
        ),
        "expected MissingStep at version 2, got: {:?}",
        result
    );
    assert_eq!(registry.counters().failed(), 1);
}

/// **R-4**: a stored version newer than the current version is an
/// operator/deploy error and MUST propagate as `UpcastError::FutureVersion`
/// — it is never skippable, even under `DeadLetter`.
#[tokio::test]
async fn future_version_always_propagates_as_error() {
    let mut registry = UpcasterRegistry::new();
    registry.register(InjectFieldUpcaster {
        event_type: "ItemAdded",
        from_version: 1,
        field: "quantity",
        value: json!(1u32),
    });
    // current = 2; stored version 5 is from the "future".
    registry.with_policy(FailurePolicy::DeadLetter);

    let result: Result<Option<ItemAdded>, UpcastError> = registry.upcast_and_deserialize(
        "ItemAdded",
        5,
        Uuid::new_v4(),
        Uuid::new_v4(),
        Some(json!({ "sku": "X", "quantity": 1, "warehouse": "main" })),
    );
    assert!(
        matches!(
            result,
            Err(UpcastError::FutureVersion {
                stored: 5,
                current: 2,
                ..
            })
        ),
        "expected FutureVersion(stored=5, current=2), got: {:?}",
        result
    );
    assert_eq!(
        registry.counters().dead_lettered(),
        0,
        "FutureVersion must never be dead-lettered"
    );
}

/// **R-10/R-13**: under `FailurePolicy::Fail` (the default), any
/// deserialization failure aborts loudly with an `Err`, never silently
/// drops the event.
#[tokio::test]
async fn fail_policy_aborts_loudly_on_deserialization_error() {
    let registry = UpcasterRegistry::new();
    // No upcaster; payload missing `quantity` and `warehouse` → serde error.
    let result: Result<Option<ItemAdded>, UpcastError> = registry.upcast_and_deserialize(
        "ItemAdded",
        1,
        Uuid::new_v4(),
        Uuid::new_v4(),
        Some(json!({ "sku": "X" })),
    );
    assert!(
        matches!(result, Err(UpcastError::Deserialize { .. })),
        "Fail policy must surface a deserialization error as Err, got: {:?}",
        result
    );
    assert_eq!(registry.counters().failed(), 1);
    assert_eq!(registry.counters().dead_lettered(), 0);
}

/// **R-11**: under `FailurePolicy::DeadLetter`, a failing event is captured
/// by the sink, counted, logged, and the stream continues (`Ok(None)`).
/// Silent event loss is impossible: the sink always receives the raw payload.
#[tokio::test]
async fn dead_letter_policy_captures_counts_and_continues() {
    let (sink, captured) = RecordingSink::new();
    let mut registry = UpcasterRegistry::new();
    registry
        .with_policy(FailurePolicy::DeadLetter)
        .with_dead_letter_sink(sink);

    // Payload missing `quantity` and `warehouse` → fails after upcasting (no steps).
    let result: Result<Option<ItemAdded>, UpcastError> = registry.upcast_and_deserialize(
        "ItemAdded",
        1,
        Uuid::new_v4(),
        Uuid::new_v4(),
        Some(json!({ "sku": "X" })),
    );

    // Stream continues: Ok(None).
    assert!(
        matches!(result, Ok(None)),
        "DeadLetter policy must return Ok(None) to continue the stream, got: {:?}",
        result
    );
    assert_eq!(
        registry.counters().dead_lettered(),
        1,
        "dead-lettered counter"
    );
    assert_eq!(
        registry.counters().failed(),
        0,
        "failed counter must stay 0"
    );
    assert_eq!(
        registry.counters().succeeded(),
        0,
        "succeeded counter must stay 0"
    );

    // Give the fire-and-forget task time to write to the sink.
    tokio::time::sleep(tokio::time::Duration::from_millis(30)).await;

    let guard = captured.lock().unwrap();
    assert_eq!(
        guard.len(),
        1,
        "sink must receive exactly one dead-lettered event"
    );
    assert_eq!(guard[0].event_type, "ItemAdded");
    assert_eq!(guard[0].stored_version, 1);
}

/// **R-11** (no silent loss): after multiple dead-lettered events the sink
/// holds one entry per event — none are dropped silently.
#[tokio::test]
async fn dead_letter_policy_never_silently_loses_events() {
    let (sink, captured) = RecordingSink::new();
    let mut registry = UpcasterRegistry::new();
    registry
        .with_policy(FailurePolicy::DeadLetter)
        .with_dead_letter_sink(sink);

    // Three consecutive failures (missing fields).
    for _ in 0..3 {
        let _: Result<Option<ItemAdded>, _> = registry.upcast_and_deserialize(
            "ItemAdded",
            1,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Some(json!({ "sku": "X" })),
        );
    }

    assert_eq!(
        registry.counters().dead_lettered(),
        3,
        "three dead-lettered events"
    );
    assert_eq!(registry.counters().failed(), 0);

    tokio::time::sleep(tokio::time::Duration::from_millis(30)).await;
    let guard = captured.lock().unwrap();
    assert_eq!(
        guard.len(),
        3,
        "sink must capture all three events — none silently lost"
    );
}
