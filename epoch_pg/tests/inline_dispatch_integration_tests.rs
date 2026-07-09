//! Integration tests for `DispatchMode::Inline` on `PgEventBus`.
//!
//! Inline mode skips the LISTEN/NOTIFY background task and calls subscribers
//! synchronously from `publish`. These tests verify:
//! 1. A subscriber sees an event as soon as `store_event` returns.
//! 2. Re-entrant publishes from inside a handler are queued (FIFO), not
//!    recursively dispatched — so a saga that emits an event of its own
//!    subscribed type does not deadlock on its observer mutex.
//! 3. Priority ordering is honored.
//! 4. Cross-bus cascades dispatch synchronously: a handler on bus A that
//!    publishes to bus B triggers a nested drain of B (not a parked queue
//!    entry), and a B handler publishing back to A defers to A's outer drain
//!    without deadlocking.

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
    // A -> B -> A cycle: bus A's saga forwards to bus B, whose saga forwards
    // back to bus A. The publish back to A must be recognized as re-entrant
    // for A (A is still draining further up the same task) and queued for
    // A's outer drain — not nested (mutex deadlock) and not lost.
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

    // Cascade: a_saga(A) -> nested drain of B: b_saga(B) -> publish back to
    // A is queued (A still draining) -> b_saga finishes -> a_saga finishes
    // -> A's outer drain processes the queued B -> a_saga(B) logs it.
    let order = order.lock().await.clone();
    assert_eq!(
        order,
        vec!["a_saga", "b_saga", "forwarded", "forwarded", "a_saga"]
    );
}
