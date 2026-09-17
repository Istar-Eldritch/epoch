//! Spec 0031 Phase 5 (R11, R16): `EventBus::unsubscribe` is a defaulted trait
//! method. This pins that a minimal third-party `EventBus` impl that predates
//! the method — overriding neither `unsubscribe` nor anything new — still
//! compiles, and that the default body's `Ok(false)` is reachable.

use epoch_core::event::{Event, EventData};
use epoch_core::event_store::{EventBus, EventObserver};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct TestEv;

impl EventData for TestEv {
    fn event_type(&self) -> &'static str {
        "TestEv"
    }
}

/// A minimal bus that only implements the pre-Phase-5 trait surface
/// (`publish` + `subscribe`), exercising the default `unsubscribe` body.
struct LegacyBus;

#[derive(Debug, thiserror::Error)]
#[error("legacy bus error")]
struct LegacyBusError;

impl EventBus for LegacyBus {
    type EventType = TestEv;
    type Error = LegacyBusError;

    fn publish<'a>(
        &'a self,
        _event: Arc<Event<Self::EventType>>,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }

    fn subscribe<T>(
        &self,
        _projector: T,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send>>
    where
        T: EventObserver<Self::EventType> + Send + Sync + 'static,
    {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn default_unsubscribe_reports_unsupported_as_ok_false() {
    let bus = LegacyBus;
    let removed = bus.unsubscribe("anything").await.unwrap();
    assert!(!removed);
}
