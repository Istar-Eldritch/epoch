//! Contract test: verifies that [`InMemoryEventStore`] satisfies the
//! [`epoch_core::event_store::EventStoreBackend::store_events`] atomicity contract.
//!
//! Uses the portable helper from `epoch_core::testing` so the same contract
//! can be run against any backend.

use epoch_core::event::{EnumConversionError, Event, EventData};
use epoch_mem::{InMemoryEventBus, InMemoryEventStore};

/// Minimal event type for the atomicity contract test.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ContractTestEvent {
    value: String,
}

impl EventData for ContractTestEvent {
    fn event_type(&self) -> &'static str {
        "ContractTestEvent"
    }
}

// Identity TryFrom required by InMemoryEventStore's EventBus bound.
impl TryFrom<&ContractTestEvent> for ContractTestEvent {
    type Error = EnumConversionError;
    fn try_from(value: &ContractTestEvent) -> Result<Self, Self::Error> {
        Ok(value.clone())
    }
}

/// Verifies [`InMemoryEventStore`] satisfies the atomicity contract for `store_events`.
#[tokio::test]
async fn in_memory_event_store_satisfies_atomicity_contract() {
    let bus = InMemoryEventBus::<ContractTestEvent>::new();
    let store = InMemoryEventStore::new(bus);

    epoch_core::testing::verify_store_events_atomicity(store, |stream_id, stream_version| {
        Event::<ContractTestEvent>::builder()
            .stream_id(stream_id)
            .stream_version(stream_version)
            .event_type("ContractTestEvent".to_string())
            .data(Some(ContractTestEvent {
                value: format!("v{stream_version}"),
            }))
            .build()
            .unwrap()
    })
    .await;
}
