//! Contract test: verifies that [`PgEventStore`] satisfies the
//! [`epoch_core::event_store::EventStoreBackend::store_events`] atomicity contract.
//!
//! Uses the portable helper from `epoch_core::testing` so the same contract
//! can be run against any backend.
//!
//! This test is DB-gated: it skips gracefully when Postgres is unreachable
//! (set `EPOCH_REQUIRE_DB=1` to turn a missing database into a hard failure).

mod common;

use epoch_core::event::{Event, EventData};
use epoch_mem::InMemoryEventBus;
use epoch_pg::{Migrator, event_store::PgEventStore};
use serde::{Deserialize, Serialize};
use serial_test::serial;

/// Minimal event type for the atomicity contract test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
enum AtomicityTestEvent {
    AtomicityTest { value: String },
}

impl EventData for AtomicityTestEvent {
    fn event_type(&self) -> &'static str {
        "AtomicityTest"
    }
}

/// Verifies [`PgEventStore`] satisfies the atomicity contract for `store_events`.
#[tokio::test]
#[serial]
async fn pg_event_store_satisfies_atomicity_contract() {
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };

    Migrator::new(pool.clone())
        .run()
        .await
        .expect("Failed to run migrations");

    let event_bus = InMemoryEventBus::<AtomicityTestEvent>::new();
    let event_store = PgEventStore::new(pool, event_bus);

    epoch_core::testing::verify_store_events_atomicity(event_store, |stream_id, stream_version| {
        Event::<AtomicityTestEvent>::builder()
            .stream_id(stream_id)
            .stream_version(stream_version)
            .event_type("AtomicityTest".to_string())
            .data(Some(AtomicityTestEvent::AtomicityTest {
                value: format!("v{stream_version}"),
            }))
            .build()
            .unwrap()
    })
    .await;
}
