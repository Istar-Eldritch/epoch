//! Integration tests for `DispatchMode::Inline` on `PgEventBus`.
//!
//! Inline mode skips the LISTEN/NOTIFY background task and calls subscribers
//! synchronously from `publish`. These tests verify:
//! 1. A subscriber sees an event as soon as `store_event` returns.
//! 2. Re-entrant publishes from inside a handler are queued (FIFO), not
//!    recursively dispatched — so a saga that emits an event of its own
//!    subscribed type does not deadlock on its observer mutex.
//! 3. Priority ordering is honored.
//! 4. Cross-bus cascades are dispatched, not silently dropped: a handler on
//!    bus A that publishes to bus B is deferred until bus A's own queue is
//!    empty, then run (not a parked queue entry that nothing ever drains),
//!    and a B handler publishing back to A defers to A's outer drain without
//!    deadlocking.
//! 5. A cross-bus cascade never observes a same-stream command's state
//!    before that command's own `persist_state()` has run: deferring (rather
//!    than immediately nesting) the cross-bus dispatch preserves the same
//!    "only after persist_state" guarantee that same-bus re-entrance already
//!    had.

mod common;

use async_trait::async_trait;
use epoch_core::prelude::*;
use epoch_derive::EventData;
use epoch_pg::Migrator;
use epoch_pg::event_bus::{DispatchMode, PgEventBus, ReliableDeliveryConfig};
use epoch_pg::event_store::PgEventStore;
use serial_test::serial;
use sqlx::PgPool;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, EventData)]
enum TestEvent {
    A,
    B,
}

fn ev(stream_id: Uuid, version: u64, data: TestEvent) -> Event<TestEvent> {
    let ty = match &data {
        TestEvent::A => "A",
        TestEvent::B => "B",
    };
    Event::<TestEvent>::builder()
        .stream_id(stream_id)
        .event_type(ty.to_string())
        .stream_version(version)
        .data(Some(data))
        .build()
        .unwrap()
}

async fn setup_inline() -> Option<(
    PgPool,
    PgEventBus<TestEvent>,
    PgEventStore<PgEventBus<TestEvent>>,
)> {
    let _ = env_logger::builder().is_test(true).try_init();
    let pool = common::try_get_pg_pool().await?;
    Migrator::new(pool.clone()).run().await.expect("migrate");

    let channel = format!("inline_test_{}", Uuid::new_v4().simple());
    let cfg = ReliableDeliveryConfig {
        dispatch_mode: DispatchMode::Inline,
        ..Default::default()
    };
    let bus = PgEventBus::with_config(pool.clone(), channel, cfg);
    let store = PgEventStore::new(pool.clone(), bus.clone());
    // start_listener is a no-op in Inline mode; call it to confirm.
    bus.start_listener().await.expect("noop start_listener");
    Some((pool, bus, store))
}

/// A subscriber that records every event it sees, in order.
struct Recorder {
    id: String,
    seen: Arc<Mutex<Vec<TestEvent>>>,
    priority: u8,
}

impl Recorder {
    fn new(id: &str, priority: u8) -> (Self, Arc<Mutex<Vec<TestEvent>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                id: id.to_string(),
                seen: seen.clone(),
                priority,
            },
            seen,
        )
    }
}

impl epoch_core::SubscriberId for Recorder {
    fn subscriber_id(&self) -> &str {
        &self.id
    }
}

#[async_trait]
impl EventObserver<TestEvent> for Recorder {
    async fn on_event(
        &self,
        event: Arc<Event<TestEvent>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let Some(data) = event.data.clone() {
            self.seen.lock().await.push(data);
        }
        Ok(())
    }

    fn priority(&self) -> u8 {
        self.priority
    }
}

#[tokio::test]
#[serial]
async fn inline_mode_dispatches_synchronously_on_store_event() {
    let Some((_pool, bus, store)) = setup_inline().await else {
        return;
    };

    let (recorder, seen) = Recorder::new("inline:recorder:sync", 0);
    bus.subscribe(recorder).await.expect("subscribe");

    let stream_id = Uuid::new_v4();
    store
        .store_event(ev(stream_id, 1, TestEvent::A))
        .await
        .expect("store_event");

    // No sleep, no polling: by the time store_event returns the subscriber
    // must have processed the event.
    let seen = seen.lock().await.clone();
    assert_eq!(seen, vec![TestEvent::A]);
}

#[tokio::test]
#[serial]
async fn inline_mode_honors_priority_order() {
    let Some((_pool, bus, store)) = setup_inline().await else {
        return;
    };

    let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));

    struct Tagger {
        id: String,
        tag: &'static str,
        priority: u8,
        order: Arc<Mutex<Vec<&'static str>>>,
    }
    impl epoch_core::SubscriberId for Tagger {
        fn subscriber_id(&self) -> &str {
            &self.id
        }
    }
    #[async_trait]
    impl EventObserver<TestEvent> for Tagger {
        async fn on_event(
            &self,
            _e: Arc<Event<TestEvent>>,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            self.order.lock().await.push(self.tag);
            Ok(())
        }
        fn priority(&self) -> u8 {
            self.priority
        }
    }

    // Register the saga first to ensure ordering comes from priority, not
    // registration order.
    bus.subscribe(Tagger {
        id: "inline:tagger:saga".into(),
        tag: "saga",
        priority: 100,
        order: order.clone(),
    })
    .await
    .expect("subscribe saga");
    bus.subscribe(Tagger {
        id: "inline:tagger:projection".into(),
        tag: "projection",
        priority: 0,
        order: order.clone(),
    })
    .await
    .expect("subscribe projection");

    store
        .store_event(ev(Uuid::new_v4(), 1, TestEvent::A))
        .await
        .expect("store");

    let order = order.lock().await.clone();
    assert_eq!(order, vec!["projection", "saga"]);
}

#[tokio::test]
#[serial]
async fn inline_mode_queues_reentrant_publishes_instead_of_recursing() {
    // Models the credit_reservation saga's hazard: a single subscriber
    // listens for both A and B. On A it triggers a publish of B. With naive
    // recursive dispatch this re-enters the subscriber's mutex while it is
    // still held processing A, deadlocking. The queue model must process A
    // first, return, then process B.
    let Some((_pool, bus, store)) = setup_inline().await else {
        return;
    };

    let stream_id = Uuid::new_v4();
    let store_arc = Arc::new(store);
    let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));

    struct ReentrantSaga {
        id: String,
        order: Arc<Mutex<Vec<&'static str>>>,
        store: Arc<PgEventStore<PgEventBus<TestEvent>>>,
        stream_id: Uuid,
    }
    impl epoch_core::SubscriberId for ReentrantSaga {
        fn subscriber_id(&self) -> &str {
            &self.id
        }
    }
    #[async_trait]
    impl EventObserver<TestEvent> for ReentrantSaga {
        async fn on_event(
            &self,
            event: Arc<Event<TestEvent>>,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
            match event.data.as_ref() {
                Some(TestEvent::A) => {
                    self.order.lock().await.push("A:start");
                    // Trigger a re-entrant publish of B from inside this
                    // handler. Naive recursive dispatch would deadlock here.
                    self.store
                        .store_event(ev(self.stream_id, 2, TestEvent::B))
                        .await
                        .expect("store B");
                    self.order.lock().await.push("A:end");
                }
                Some(TestEvent::B) => {
                    self.order.lock().await.push("B");
                }
                None => {}
            }
            Ok(())
        }
    }

    bus.subscribe(ReentrantSaga {
        id: "inline:reentrant_saga".into(),
        order: order.clone(),
        store: store_arc.clone(),
        stream_id,
    })
    .await
    .expect("subscribe");

    // Top-level publish of A. Must:
    //  1. invoke saga(A) to completion (no deadlock on the saga's mutex when
    //     it tries to publish B)
    //  2. then invoke saga(B) after saga(A) has returned
    store_arc
        .store_event(ev(stream_id, 1, TestEvent::A))
        .await
        .expect("store A");

    let order = order.lock().await.clone();
    assert_eq!(order, vec!["A:start", "A:end", "B"]);
}

/// A subscriber on one bus that stores (and thus publishes) to another bus's
/// event store from inside its handler.
struct CrossBusForwarder {
    id: String,
    order: Arc<Mutex<Vec<&'static str>>>,
    other_store: Arc<PgEventStore<PgEventBus<TestEvent>>>,
    stream_id: Uuid,
    /// Event data that triggers the forward (any other event is only logged).
    forward_on: TestEvent,
    /// What to store on the other bus when triggered.
    forward_as: TestEvent,
    tag: &'static str,
}
impl epoch_core::SubscriberId for CrossBusForwarder {
    fn subscriber_id(&self) -> &str {
        &self.id
    }
}
#[async_trait]
impl EventObserver<TestEvent> for CrossBusForwarder {
    async fn on_event(
        &self,
        event: Arc<Event<TestEvent>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.order.lock().await.push(self.tag);
        if event.data.as_ref() == Some(&self.forward_on) {
            self.other_store
                .store_event(ev(self.stream_id, 1, self.forward_as.clone()))
                .await
                .expect("cross-bus store");
            self.order.lock().await.push("forwarded");
        }
        Ok(())
    }
}

#[tokio::test]
#[serial]
async fn inline_mode_dispatches_cross_bus_cascades_synchronously() {
    // Models the multi-bus monolith wiring: a saga subscribed to bus A
    // handles an event by dispatching a command whose events are stored and
    // published on bus B. Bus B's subscribers must run as part of the
    // cascade — before the top-level publish on A returns — not sit parked
    // on B's queue until some unrelated future publish on B.
    let Some((pool, bus_a, store_a)) = setup_inline().await else {
        return;
    };

    // Second, independent bus sharing the same events table.
    let channel_b = format!("inline_test_{}", Uuid::new_v4().simple());
    let cfg_b = ReliableDeliveryConfig {
        dispatch_mode: DispatchMode::Inline,
        ..Default::default()
    };
    let bus_b = PgEventBus::with_config(pool.clone(), channel_b, cfg_b);
    let store_b = Arc::new(PgEventStore::new(pool.clone(), bus_b.clone()));

    let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));
    let stream_b = Uuid::new_v4();

    // Bus A: saga that forwards A-events to bus B.
    bus_a
        .subscribe(CrossBusForwarder {
            id: "inline:cross:a_saga".into(),
            order: order.clone(),
            other_store: store_b.clone(),
            stream_id: stream_b,
            forward_on: TestEvent::A,
            forward_as: TestEvent::B,
            tag: "a_saga",
        })
        .await
        .expect("subscribe a saga");

    // Bus B: plain recorder.
    let (recorder_b, seen_b) = Recorder::new("inline:cross:b_recorder", 0);
    bus_b.subscribe(recorder_b).await.expect("subscribe b");

    store_a
        .store_event(ev(Uuid::new_v4(), 1, TestEvent::A))
        .await
        .expect("store A");

    // By the time the top-level store on bus A returns, bus B's subscriber
    // must have run (nested inline drain), and the forwarding handler must
    // have completed.
    let seen_b = seen_b.lock().await.clone();
    assert_eq!(seen_b, vec![TestEvent::B]);
    let order = order.lock().await.clone();
    assert_eq!(order, vec!["a_saga", "forwarded"]);
}

#[tokio::test]
#[serial]
async fn inline_mode_cross_bus_cycle_defers_to_outer_drain_without_deadlock() {
    // A -> B -> A cycle: bus A's saga forwards to bus B (deferred, not run
    // immediately), whose saga forwards back to bus A. The publish back to A
    // must be recognized as re-entrant for A (A is still draining further up
    // the same task) and queued for A's outer drain — not nested (mutex
    // deadlock) and not lost.
    let Some((pool, bus_a, store_a)) = setup_inline().await else {
        return;
    };
    let store_a = Arc::new(store_a);

    let channel_b = format!("inline_test_{}", Uuid::new_v4().simple());
    let cfg_b = ReliableDeliveryConfig {
        dispatch_mode: DispatchMode::Inline,
        ..Default::default()
    };
    let bus_b = PgEventBus::with_config(pool.clone(), channel_b, cfg_b);
    let store_b = Arc::new(PgEventStore::new(pool.clone(), bus_b.clone()));

    let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));

    // Bus A: on A, forward B to bus B. (On the queued follow-up B event it
    // only logs — forward_on doesn't match.)
    bus_a
        .subscribe(CrossBusForwarder {
            id: "inline:cycle:a_saga".into(),
            order: order.clone(),
            other_store: store_b.clone(),
            stream_id: Uuid::new_v4(),
            forward_on: TestEvent::A,
            forward_as: TestEvent::B,
            tag: "a_saga",
        })
        .await
        .expect("subscribe a saga");

    // Bus B: on B, forward back to bus A as B (so A's saga logs it without
    // re-forwarding, terminating the cascade).
    bus_b
        .subscribe(CrossBusForwarder {
            id: "inline:cycle:b_saga".into(),
            order: order.clone(),
            other_store: store_a.clone(),
            stream_id: Uuid::new_v4(),
            forward_on: TestEvent::B,
            forward_as: TestEvent::B,
            tag: "b_saga",
        })
        .await
        .expect("subscribe b saga");

    store_a
        .store_event(ev(Uuid::new_v4(), 1, TestEvent::A))
        .await
        .expect("store A");

    // Cascade: a_saga(A) runs and *defers* its forward to B (returns
    // immediately, "forwarded" logs right away) -> A's own queue empties ->
    // the deferred B-forward is popped and run: b_saga(B) runs and forwards
    // back to A, which is re-entrant (A still draining) so it's queued on
    // A's own queue rather than run immediately -> b_saga finishes -> A's
    // outer loop re-checks its own queue, finds the queued B, and runs
    // a_saga(B) (forward_on doesn't match, so it only logs).
    let order = order.lock().await.clone();
    assert_eq!(
        order,
        vec!["a_saga", "forwarded", "b_saga", "forwarded", "a_saga"]
    );
}

// =============================================================================
// Cross-bus cascade must not observe a same-stream command's state before
// that command's own persist_state() has run (the catacloud regression).
// =============================================================================

/// Minimal event-sourced counter, wired to `PgEventStore<PgEventBus<CounterEvent>>`
/// and driven through the *plain* (non-transactional) `Aggregate::handle()`
/// path — the one where `store_events()` (and thus, in Inline mode,
/// synchronous subscriber dispatch) runs before `persist_state()`. Deliberately
/// distinct from `transaction_integration_tests.rs`'s `CounterAggregate`
/// (which uses `InMemoryEventBus` and is driven through
/// `TransactionalAggregate`, where state is persisted before commit/publish
/// and this hazard cannot occur).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, EventData)]
enum CounterEvent {
    Created { id: Uuid, value: i64 },
    Incremented { id: Uuid, amount: i64 },
}

impl TryFrom<&CounterEvent> for CounterEvent {
    type Error = epoch_core::event::EnumConversionError;
    fn try_from(value: &CounterEvent) -> Result<Self, Self::Error> {
        Ok(value.clone())
    }
}

#[derive(Debug, Clone)]
enum CounterCommand {
    Create { id: Uuid, value: i64 },
    Increment { id: Uuid, amount: i64 },
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
struct CounterState {
    id: Uuid,
    value: i64,
    version: i64,
}

impl epoch_core::event_applicator::EventApplicatorState for CounterState {
    fn get_id(&self) -> &Uuid {
        &self.id
    }
}

impl epoch_core::aggregate::AggregateState for CounterState {
    fn get_version(&self) -> u64 {
        self.version as u64
    }
    fn set_version(&mut self, version: u64) {
        self.version = version as i64;
    }
}

#[async_trait]
impl epoch_pg::state_store::PgState for CounterState {
    async fn find_by_id<'a, T>(id: Uuid, executor: T) -> Result<Option<Self>, sqlx::Error>
    where
        T: sqlx::PgExecutor<'a>,
    {
        sqlx::query_as("SELECT id, value, version FROM inline_counter_states WHERE id = $1")
            .bind(id)
            .fetch_optional(executor)
            .await
    }

    async fn find_by_id_for_update<'a, T>(
        id: Uuid,
        executor: T,
    ) -> Result<Option<Self>, sqlx::Error>
    where
        T: sqlx::PgExecutor<'a>,
    {
        sqlx::query_as(
            "SELECT id, value, version FROM inline_counter_states WHERE id = $1 FOR UPDATE",
        )
        .bind(id)
        .fetch_optional(executor)
        .await
    }

    async fn upsert<'a, T>(id: Uuid, state: &'a Self, executor: T) -> Result<(), sqlx::Error>
    where
        T: sqlx::PgExecutor<'a>,
    {
        sqlx::query(
            "INSERT INTO inline_counter_states (id, value, version) VALUES ($1, $2, $3) \
             ON CONFLICT (id) DO UPDATE SET value = $2, version = $3",
        )
        .bind(id)
        .bind(state.value)
        .bind(state.version)
        .execute(executor)
        .await?;
        Ok(())
    }

    async fn delete<'a, T>(id: Uuid, executor: T) -> Result<(), sqlx::Error>
    where
        T: sqlx::PgExecutor<'a>,
    {
        sqlx::query("DELETE FROM inline_counter_states WHERE id = $1")
            .bind(id)
            .execute(executor)
            .await?;
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
enum CounterError {
    #[error("Counter already exists")]
    AlreadyExists,
    #[error("Counter not found")]
    NotFound,
}

struct CounterAggregate {
    event_store: PgEventStore<PgEventBus<CounterEvent>>,
    state_store: epoch_pg::state_store::PgStateStore<CounterState>,
}

impl CounterAggregate {
    fn new(pool: PgPool, event_store: PgEventStore<PgEventBus<CounterEvent>>) -> Self {
        Self {
            event_store,
            state_store: epoch_pg::state_store::PgStateStore::new(pool),
        }
    }
}

impl epoch_core::event_applicator::EventApplicator<CounterEvent> for CounterAggregate {
    type State = CounterState;
    type StateStore = epoch_pg::state_store::PgStateStore<CounterState>;
    type EventType = CounterEvent;
    type ApplyError = std::convert::Infallible;

    fn get_state_store(&self) -> Self::StateStore {
        self.state_store.clone()
    }

    fn apply(
        &self,
        state: Option<Self::State>,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::ApplyError> {
        match &event.data {
            Some(CounterEvent::Created { id, value }) => Ok(Some(CounterState {
                id: *id,
                value: *value,
                version: 0,
            })),
            Some(CounterEvent::Incremented { amount, .. }) => {
                let mut state = state.expect("state must exist for Incremented");
                state.value += amount;
                Ok(Some(state))
            }
            None => Ok(state),
        }
    }
}

#[async_trait]
impl Aggregate<CounterEvent> for CounterAggregate {
    type CommandData = CounterCommand;
    type CommandCredentials = ();
    type Command = CounterCommand;
    type EventStore = PgEventStore<PgEventBus<CounterEvent>>;
    type AggregateError = CounterError;

    fn get_event_store(&self) -> Self::EventStore {
        self.event_store.clone()
    }

    async fn handle_command(
        &self,
        state: &Option<Self::State>,
        command: Command<Self::Command, Self::CommandCredentials>,
    ) -> Result<Vec<Event<CounterEvent>>, Self::AggregateError> {
        match command.data {
            CounterCommand::Create { id, value } => {
                if state.is_some() {
                    return Err(CounterError::AlreadyExists);
                }
                Ok(vec![
                    Event::<CounterEvent>::builder()
                        .stream_id(id)
                        .event_type("Created".to_string())
                        .data(Some(CounterEvent::Created { id, value }))
                        .build()
                        .unwrap(),
                ])
            }
            CounterCommand::Increment { id, amount } => {
                // The bug this test guards against: if this command runs
                // before the Create command's own persist_state() has
                // completed, `state` is None here even though Created was
                // already durably stored and published.
                if state.is_none() {
                    return Err(CounterError::NotFound);
                }
                Ok(vec![
                    Event::<CounterEvent>::builder()
                        .stream_id(id)
                        .event_type("Incremented".to_string())
                        .data(Some(CounterEvent::Incremented { id, amount }))
                        .build()
                        .unwrap(),
                ])
            }
        }
    }
}

/// Trigger event on a separate bus: a saga reacting to `Fire` creates the
/// counter on `CounterAggregate`'s bus — the cross-bus hop under test.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, EventData)]
enum TriggerEvent {
    Fire { counter_id: Uuid },
}

struct CreateOnFireSaga {
    counter: Arc<CounterAggregate>,
}
impl epoch_core::SubscriberId for CreateOnFireSaga {
    fn subscriber_id(&self) -> &str {
        "inline:regression:create_on_fire"
    }
}
#[async_trait]
impl EventObserver<TriggerEvent> for CreateOnFireSaga {
    async fn on_event(
        &self,
        event: Arc<Event<TriggerEvent>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let Some(TriggerEvent::Fire { counter_id }) = event.data.as_ref() {
            self.counter
                .handle(Command::new(
                    *counter_id,
                    CounterCommand::Create {
                        id: *counter_id,
                        value: 0,
                    },
                    Some(()),
                    None,
                ))
                .await?;
        }
        Ok(())
    }
}

/// Reacts to `Created` (on the counter's own bus) by immediately incrementing
/// the *same* stream. This is exactly the notification_creator -> delivery
/// saga shape from the catacloud regression: a saga subscribed to the target
/// aggregate's own bus, reacting to the event a cross-bus cascade just
/// produced, issuing a second command against that same aggregate.
struct IncrementOnCreatedSaga {
    counter: Arc<CounterAggregate>,
}
impl epoch_core::SubscriberId for IncrementOnCreatedSaga {
    fn subscriber_id(&self) -> &str {
        "inline:regression:increment_on_created"
    }
}
#[async_trait]
impl EventObserver<CounterEvent> for IncrementOnCreatedSaga {
    async fn on_event(
        &self,
        event: Arc<Event<CounterEvent>>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if let Some(CounterEvent::Created { id, .. }) = event.data.as_ref() {
            self.counter
                .handle(Command::new(
                    *id,
                    CounterCommand::Increment { id: *id, amount: 1 },
                    Some(()),
                    None,
                ))
                .await?;
        }
        Ok(())
    }
}

#[tokio::test]
#[serial]
async fn inline_mode_cross_bus_cascade_sees_persisted_state_of_same_stream_command() {
    let Some(pool) = common::try_get_pg_pool().await else {
        return;
    };
    let _ = env_logger::builder().is_test(true).try_init();
    Migrator::new(pool.clone()).run().await.expect("migrate");
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS inline_counter_states (
            id UUID PRIMARY KEY,
            value BIGINT NOT NULL,
            version BIGINT NOT NULL
        )",
    )
    .execute(&pool)
    .await
    .expect("create inline_counter_states");

    let counter_channel = format!("inline_counter_{}", Uuid::new_v4().simple());
    let counter_bus = PgEventBus::<CounterEvent>::with_config(
        pool.clone(),
        counter_channel,
        ReliableDeliveryConfig {
            dispatch_mode: DispatchMode::Inline,
            ..Default::default()
        },
    );
    let counter_event_store = PgEventStore::new(pool.clone(), counter_bus.clone());
    let counter = Arc::new(CounterAggregate::new(pool.clone(), counter_event_store));

    counter_bus
        .subscribe(IncrementOnCreatedSaga {
            counter: counter.clone(),
        })
        .await
        .expect("subscribe increment saga");

    let trigger_channel = format!("inline_trigger_{}", Uuid::new_v4().simple());
    let trigger_bus = PgEventBus::<TriggerEvent>::with_config(
        pool.clone(),
        trigger_channel,
        ReliableDeliveryConfig {
            dispatch_mode: DispatchMode::Inline,
            ..Default::default()
        },
    );
    let trigger_event_store = PgEventStore::new(pool.clone(), trigger_bus.clone());
    trigger_bus
        .subscribe(CreateOnFireSaga {
            counter: counter.clone(),
        })
        .await
        .expect("subscribe create saga");

    let counter_id = Uuid::new_v4();

    // Top-level publish on the trigger bus. Cascades: CreateOnFireSaga
    // (cross-bus) creates the counter, then IncrementOnCreatedSaga (same-bus
    // as the counter) increments it. If the cross-bus hop ran immediately
    // instead of deferring, the increment would race the create's own
    // persist_state() and fail with CounterError::NotFound.
    trigger_event_store
        .store_event(
            Event::<TriggerEvent>::builder()
                .stream_id(counter_id)
                .event_type("Fire".to_string())
                .data(Some(TriggerEvent::Fire { counter_id }))
                .build()
                .unwrap(),
        )
        .await
        .expect("store Fire — cross-bus cascade must succeed end to end");

    let state: CounterState =
        sqlx::query_as("SELECT id, value, version FROM inline_counter_states WHERE id = $1")
            .bind(counter_id)
            .fetch_one(&pool)
            .await
            .expect("counter state must exist and reflect both Create and Increment");
    assert_eq!(state.value, 1, "0 (Created) + 1 (Incremented) = 1");
    assert_eq!(
        state.version, 2,
        "stream_version starts at 1: Created=1, Incremented=2"
    );

    sqlx::query("DROP TABLE IF EXISTS inline_counter_states CASCADE")
        .execute(&pool)
        .await
        .ok();
}
