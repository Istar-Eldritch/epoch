//! T0: FailureMode trait surface tests (P1)
//!
//! Verifies that FailureMode defaults to FailOpen on EventObserver, Projection, and Saga,
//! and that FailClosed propagates correctly through ProjectionHandler, SagaHandler,
//! SagaAdapter, and the Arc<S> blanket Saga impl.

use async_trait::async_trait;
use epoch_core::SubscriberId;
use epoch_core::event::{EnumConversionError, Event, EventData};
use epoch_core::event_applicator::{EventApplicator, EventApplicatorState};
use epoch_core::event_store::{EventObserver, FailureMode};
use epoch_core::projection::{Projection, ProjectionHandler};
use epoch_core::saga::{Saga, SagaAdapter, SagaHandler};
use epoch_core::state_store::StateStoreBackend;
use std::sync::Arc;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Shared event + state types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
enum TestEv {
    Happened,
}

impl EventData for TestEv {
    fn event_type(&self) -> &'static str {
        "Happened"
    }
}

impl TryFrom<&TestEv> for TestEv {
    type Error = EnumConversionError;
    fn try_from(v: &TestEv) -> Result<Self, Self::Error> {
        Ok(v.clone())
    }
}

#[derive(Debug, Clone)]
struct NoState {
    id: Uuid,
}

impl Default for NoState {
    fn default() -> Self {
        Self { id: Uuid::nil() }
    }
}

impl EventApplicatorState for NoState {
    fn get_id(&self) -> &Uuid {
        &self.id
    }
}

#[derive(Debug, thiserror::Error)]
#[error("no error")]
struct NoError;

struct NoStore;

#[async_trait]
impl StateStoreBackend<NoState> for NoStore {
    type Error = NoError;
    async fn get_state(&self, _: Uuid) -> Result<Option<NoState>, Self::Error> {
        Ok(None)
    }
    async fn persist_state(&mut self, _: Uuid, _: NoState) -> Result<(), Self::Error> {
        Ok(())
    }
    async fn delete_state(&mut self, _: Uuid) -> Result<(), Self::Error> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ForwardingProjection — configurable failure_mode for testing
// ---------------------------------------------------------------------------

struct ForwardingProjection {
    mode: FailureMode,
}

impl ForwardingProjection {
    fn fail_closed() -> Self {
        Self {
            mode: FailureMode::FailClosed,
        }
    }

    fn fail_open() -> Self {
        Self {
            mode: FailureMode::FailOpen,
        }
    }
}

impl SubscriberId for ForwardingProjection {
    fn subscriber_id(&self) -> &str {
        "projection:forwarding"
    }
}

impl EventApplicator<TestEv> for ForwardingProjection {
    type State = NoState;
    type StateStore = NoStore;
    type EventType = TestEv;
    type ApplyError = NoError;

    fn get_state_store(&self) -> Self::StateStore {
        NoStore
    }

    fn apply(
        &self,
        _state: Option<Self::State>,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::ApplyError> {
        Ok(Some(NoState {
            id: event.stream_id,
        }))
    }
}

impl Projection<TestEv> for ForwardingProjection {
    fn failure_mode(&self) -> FailureMode {
        self.mode
    }
}

// ---------------------------------------------------------------------------
// ForwardingSaga — configurable failure_mode for testing
// ---------------------------------------------------------------------------

struct ForwardingSaga {
    mode: FailureMode,
}

impl ForwardingSaga {
    fn fail_closed() -> Self {
        Self {
            mode: FailureMode::FailClosed,
        }
    }
}

impl SubscriberId for ForwardingSaga {
    fn subscriber_id(&self) -> &str {
        "saga:forwarding"
    }
}

#[async_trait]
impl Saga<TestEv> for ForwardingSaga {
    type State = NoState;
    type StateStore = NoStore;
    type SagaError = NoError;
    type EventType = TestEv;

    fn get_state_store(&self) -> Self::StateStore {
        NoStore
    }

    async fn handle_event(
        &self,
        state: Self::State,
        _event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::SagaError> {
        Ok(Some(state))
    }

    fn failure_mode(&self) -> FailureMode {
        self.mode
    }
}

// ---------------------------------------------------------------------------
// Default-only Projection and Saga (no override — exercises default bodies)
// ---------------------------------------------------------------------------

struct DefaultProjection;

impl SubscriberId for DefaultProjection {
    fn subscriber_id(&self) -> &str {
        "projection:default"
    }
}

impl EventApplicator<TestEv> for DefaultProjection {
    type State = NoState;
    type StateStore = NoStore;
    type EventType = TestEv;
    type ApplyError = NoError;

    fn get_state_store(&self) -> Self::StateStore {
        NoStore
    }

    fn apply(
        &self,
        _state: Option<Self::State>,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::ApplyError> {
        Ok(Some(NoState {
            id: event.stream_id,
        }))
    }
}

impl Projection<TestEv> for DefaultProjection {}

struct DefaultSaga;

impl SubscriberId for DefaultSaga {
    fn subscriber_id(&self) -> &str {
        "saga:default"
    }
}

#[async_trait]
impl Saga<TestEv> for DefaultSaga {
    type State = NoState;
    type StateStore = NoStore;
    type SagaError = NoError;
    type EventType = TestEv;

    fn get_state_store(&self) -> Self::StateStore {
        NoStore
    }

    async fn handle_event(
        &self,
        state: Self::State,
        _event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::SagaError> {
        Ok(Some(state))
    }
}

// ---------------------------------------------------------------------------
// T0 tests
// ---------------------------------------------------------------------------

#[test]
fn t0_failure_mode_defaults_fail_open_on_event_observer_trait() {
    // A ProjectionHandler wrapping a default-only Projection exposes FailOpen
    let handler = ProjectionHandler::new(DefaultProjection);
    let obs: &dyn EventObserver<TestEv> = &handler;
    assert_eq!(obs.failure_mode(), FailureMode::FailOpen);
}

#[test]
fn t0_failure_mode_defaults_fail_open_on_projection_trait() {
    assert_eq!(DefaultProjection.failure_mode(), FailureMode::FailOpen);
}

#[test]
fn t0_failure_mode_defaults_fail_open_on_saga_trait() {
    assert_eq!(DefaultSaga.failure_mode(), FailureMode::FailOpen);
}

#[test]
fn t0_failure_mode_defaults_fail_open_through_saga_handler() {
    let handler = SagaHandler::new(DefaultSaga);
    let obs: &dyn EventObserver<TestEv> = &handler;
    assert_eq!(obs.failure_mode(), FailureMode::FailOpen);
}

#[test]
fn t0_fail_closed_forwarded_through_projection_handler() {
    let handler = ProjectionHandler::new(ForwardingProjection::fail_closed());
    let obs: &dyn EventObserver<TestEv> = &handler;
    assert_eq!(obs.failure_mode(), FailureMode::FailClosed);
}

#[test]
fn t0_fail_open_forwarded_through_projection_handler() {
    let handler = ProjectionHandler::new(ForwardingProjection::fail_open());
    let obs: &dyn EventObserver<TestEv> = &handler;
    assert_eq!(obs.failure_mode(), FailureMode::FailOpen);
}

#[test]
fn t0_fail_closed_forwarded_through_saga_handler() {
    let handler = SagaHandler::new(ForwardingSaga::fail_closed());
    let obs: &dyn EventObserver<TestEv> = &handler;
    assert_eq!(obs.failure_mode(), FailureMode::FailClosed);
}

#[test]
fn t0_fail_closed_forwarded_through_arc_blanket() {
    let saga = Arc::new(ForwardingSaga::fail_closed());
    let handler = SagaHandler::new(saga);
    let obs: &dyn EventObserver<TestEv> = &handler;
    assert_eq!(obs.failure_mode(), FailureMode::FailClosed);
}

#[test]
fn t0_fail_closed_forwarded_through_saga_adapter() {
    let saga = Arc::new(ForwardingSaga::fail_closed());
    let adapter = SagaAdapter::new(saga, "saga:adapter:t0", |e: &TestEv| Some(e.clone()));
    let obs: &dyn EventObserver<TestEv> = &adapter;
    assert_eq!(obs.failure_mode(), FailureMode::FailClosed);
}
