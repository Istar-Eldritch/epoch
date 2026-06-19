//! DB-gated end-to-end upcasting integration tests for `epoch_pg` (R-22).
//!
//! These tests verify that the Postgres read path correctly routes stored
//! payloads through an [`UpcasterRegistry`] before deserializing them into
//! the domain [`EventData`] type.
//!
//! **Scenario under test:**
//!
//! 1. A v1 JSON payload (`{ "name": "Widget" }`) is inserted directly via SQL
//!    with `schema_version = 1`.
//! 2. A `1 → 2` upcaster is registered that injects `"price": 0` into the
//!    payload.
//! 3. A `PgEventStore` is constructed with that registry
//!    (`PgEventStore::with_upcasters`).
//! 4. `read_events` is called — the registry upcasts the stored row and
//!    deserializes it into `ProductAddedV2 { name: "Widget", price: 0 }`.
//! 5. Registry counters confirm that exactly one step was applied and one
//!    event succeeded.
//!
//! This test is DB-gated: it skips gracefully when Postgres is unreachable
//! (set `EPOCH_REQUIRE_DB=1` to turn a missing database into a hard failure).

mod common;

use epoch_core::event::{Event, EventData};
use epoch_core::prelude::EventStoreBackend;
use epoch_core::upcasting::{UpcastContext, UpcastError, Upcaster, UpcasterRegistry};
use epoch_mem::InMemoryEventBus;
use epoch_pg::Migrator;
use epoch_pg::event_store::PgEventStore;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use serial_test::serial;
use std::sync::Arc;
use uuid::Uuid;

// ── Domain types ──────────────────────────────────────────────────────────────

/// The v2 shape of `ProductAdded`.
///
/// v1 stored only `name`; the v2 upcaster injects `price` with a default of
/// `0` to represent "price not recorded at time of event".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ProductAddedV2 {
    name: String,
    price: u64,
}

impl EventData for ProductAddedV2 {
    fn event_type(&self) -> &'static str {
        "ProductAdded"
    }
    fn schema_version(&self) -> u32 {
        2
    }
}

// ── Upcaster ──────────────────────────────────────────────────────────────────

/// Upcaster step: `ProductAdded` v1 → v2.
///
/// Injects `"price": 0` into any stored v1 payload so it deserializes into
/// [`ProductAddedV2`].
struct ProductAddedV1ToV2;

impl Upcaster for ProductAddedV1ToV2 {
    fn event_type(&self) -> &str {
        "ProductAdded"
    }
    fn from_version(&self) -> u32 {
        1
    }
    fn upcast(&self, _ctx: &UpcastContext<'_>, mut payload: Value) -> Result<Value, UpcastError> {
        if let Value::Object(map) = &mut payload {
            map.insert("price".to_string(), json!(0u64));
        }
        Ok(payload)
    }
}

// ── Test helpers ──────────────────────────────────────────────────────────────

/// Builds a `UpcasterRegistry` for the tests.
fn build_registry() -> UpcasterRegistry {
    let mut registry = UpcasterRegistry::new();
    registry.register(ProductAddedV1ToV2);
    registry
}

// ── R-22: end-to-end upcast replay test ──────────────────────────────────────

/// Stores a raw v1 JSON payload directly via SQL (simulating a pre-migration
/// row), then reads it back through a `PgEventStore` equipped with a 1→2
/// upcaster and asserts the deserialized value has the v2 shape.
#[tokio::test]
#[serial]
async fn v1_payload_is_upcast_and_deserialized_as_v2_on_replay() {
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };

    Migrator::new(pool.clone())
        .run()
        .await
        .expect("migrations must succeed");

    // Insert a raw v1 row directly, bypassing the Rust event store.
    let stream_id = Uuid::new_v4();
    let event_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO epoch_events \
         (id, stream_id, stream_version, event_type, data, created_at, schema_version) \
         VALUES ($1, $2, 1, 'ProductAdded', $3, NOW(), 1)",
    )
    .bind(event_id)
    .bind(stream_id)
    .bind(json!({ "name": "Widget" }))
    .execute(&pool)
    .await
    .expect("raw v1 insert must succeed");

    // Build a store with the 1→2 upcaster.
    let registry = Arc::new(build_registry());
    let bus = InMemoryEventBus::<ProductAddedV2>::new();
    let store = PgEventStore::with_upcasters(pool.clone(), bus, Arc::clone(&registry));

    // Read events — the upcaster must transform the v1 JSON to v2 before
    // deserialization.
    let mut stream = store
        .read_events(stream_id)
        .await
        .expect("read_events must succeed");

    let event = stream
        .next()
        .await
        .expect("stream must yield at least one event")
        .expect("event must not be an error");

    assert_eq!(event.stream_id, stream_id);
    assert_eq!(event.id, event_id);
    assert_eq!(event.event_type, "ProductAdded");
    assert_eq!(
        event.data,
        Some(ProductAddedV2 {
            name: "Widget".to_string(),
            price: 0,
        }),
        "v1 payload must be upcast to v2 and deserialized correctly"
    );

    // schema_version on the returned envelope should reflect the stored version (1).
    assert_eq!(
        event.schema_version, 1,
        "stored schema_version must be preserved on the returned envelope"
    );

    // The stream must not yield a second event.
    assert!(
        stream.next().await.is_none(),
        "stream must yield exactly one event"
    );

    // Registry counters verify the upcast step was applied.
    assert_eq!(registry.counters().applied(), 1, "one step (1→2) applied");
    assert_eq!(registry.counters().succeeded(), 1, "one event succeeded");
    assert_eq!(registry.counters().failed(), 0, "no failures");
}

/// Stores a new v2 event via the store (write path stamps `schema_version = 2`),
/// then reads it back — no upcaster step should be applied because the stored
/// version already equals the current version.
#[tokio::test]
#[serial]
async fn v2_event_round_trips_without_upcasting() {
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };

    Migrator::new(pool.clone())
        .run()
        .await
        .expect("migrations must succeed");

    let stream_id = Uuid::new_v4();
    let registry = Arc::new(build_registry());
    let bus = InMemoryEventBus::<ProductAddedV2>::new();
    let store = PgEventStore::with_upcasters(pool.clone(), bus, Arc::clone(&registry));

    // Store a v2 event (current shape) through the normal write path.
    let event_in = Event::<ProductAddedV2>::builder()
        .stream_id(stream_id)
        .stream_version(1)
        .event_type("ProductAdded".to_string())
        .data(Some(ProductAddedV2 {
            name: "Gadget".to_string(),
            price: 100,
        }))
        .build()
        .unwrap();

    store
        .store_event(event_in.clone())
        .await
        .expect("store_event must succeed");

    // Read it back — the registry should not apply any upcast steps because
    // the stored version (2) already equals the current version (2).
    let mut stream = store
        .read_events(stream_id)
        .await
        .expect("read_events must succeed");

    let event_out = stream
        .next()
        .await
        .expect("must yield at least one event")
        .expect("event must not error");

    assert_eq!(
        event_out.data,
        Some(ProductAddedV2 {
            name: "Gadget".to_string(),
            price: 100,
        }),
        "v2 round-trip must return the same data"
    );
    assert_eq!(
        event_out.schema_version, 2,
        "stored schema_version must be 2"
    );

    // No upcast steps should have been applied for the v2 → v2 no-op.
    assert_eq!(
        registry.counters().applied(),
        0,
        "no upcast steps should be applied for a current-version event"
    );
    assert_eq!(registry.counters().succeeded(), 1, "one successful read");
}

/// Verifies `read_last_event` also routes through the upcaster: a raw v1 row
/// should be returned with the v2 shape.
#[tokio::test]
#[serial]
async fn read_last_event_upcasts_v1_payload() {
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };

    Migrator::new(pool.clone())
        .run()
        .await
        .expect("migrations must succeed");

    let stream_id = Uuid::new_v4();

    // Insert a raw v1 row.
    sqlx::query(
        "INSERT INTO epoch_events \
         (id, stream_id, stream_version, event_type, data, created_at, schema_version) \
         VALUES ($1, $2, 1, 'ProductAdded', $3, NOW(), 1)",
    )
    .bind(Uuid::new_v4())
    .bind(stream_id)
    .bind(json!({ "name": "Doohickey" }))
    .execute(&pool)
    .await
    .expect("raw v1 insert must succeed");

    let registry = Arc::new(build_registry());
    let bus = InMemoryEventBus::<ProductAddedV2>::new();
    let store = PgEventStore::with_upcasters(pool.clone(), bus, Arc::clone(&registry));

    let event = store
        .read_last_event(stream_id)
        .await
        .expect("read_last_event must succeed")
        .expect("stream must have a last event");

    assert_eq!(
        event.data,
        Some(ProductAddedV2 {
            name: "Doohickey".to_string(),
            price: 0,
        }),
        "read_last_event must upcast v1 to v2"
    );
}

/// A raw row with `schema_version = NULL` (simulating a pre-m012 legacy row)
/// is interpreted as version `1` and upcast correctly.
#[tokio::test]
#[serial]
async fn null_schema_version_treated_as_v1_and_upcast() {
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };

    Migrator::new(pool.clone())
        .run()
        .await
        .expect("migrations must succeed");

    let stream_id = Uuid::new_v4();

    // Insert with schema_version explicitly NULL to simulate a pre-migration row.
    sqlx::query(
        "INSERT INTO epoch_events \
         (id, stream_id, stream_version, event_type, data, created_at, schema_version) \
         VALUES ($1, $2, 1, 'ProductAdded', $3, NOW(), NULL)",
    )
    .bind(Uuid::new_v4())
    .bind(stream_id)
    .bind(json!({ "name": "LegacyItem" }))
    .execute(&pool)
    .await
    .expect("raw NULL-version insert must succeed");

    let registry = Arc::new(build_registry());
    let bus = InMemoryEventBus::<ProductAddedV2>::new();
    let store = PgEventStore::with_upcasters(pool.clone(), bus, Arc::clone(&registry));

    let mut stream = store
        .read_events(stream_id)
        .await
        .expect("read_events must succeed");

    let event = stream
        .next()
        .await
        .expect("stream must yield an event")
        .expect("event must not error");

    assert_eq!(
        event.data,
        Some(ProductAddedV2 {
            name: "LegacyItem".to_string(),
            price: 0,
        }),
        "NULL schema_version must be treated as v1 and upcast to v2"
    );
    assert_eq!(
        registry.counters().applied(),
        1,
        "the 1→2 upcaster must be applied for a NULL/v1 row"
    );
}

/// Verifies the `verify_upcasting_chain` contract helper works end-to-end with
/// the registry used for these integration tests.
///
/// This test does **not** require a database — it exercises the registry
/// directly in the same way `epoch_mem` parity tests do, confirming the
/// contract helper is backend-independent.
#[tokio::test]
async fn contract_helper_verify_upcasting_chain_passes_with_pg_registry() {
    let registry = build_registry();

    epoch_core::testing::verify_upcasting_chain::<ProductAddedV2>(
        &registry,
        "ProductAdded",
        json!({ "name": "TestProduct" }),
        ProductAddedV2 {
            name: "TestProduct".to_string(),
            price: 0,
        },
    )
    .await;
}
