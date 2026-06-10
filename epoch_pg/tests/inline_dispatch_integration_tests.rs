//! Integration tests for `DispatchMode::Inline` on `PgEventBus`.
//!
//! Inline mode skips the LISTEN/NOTIFY background task and calls subscribers
//! synchronously from `publish`. These tests verify:
//! 1. A subscriber sees an event as soon as `store_event` returns.
//! 2. Re-entrant publishes from inside a handler are queued (FIFO), not
//!    recursively dispatched — so a saga that emits an event of its own
//!    subscribed type does not deadlock on its observer mutex.
//! 3. Priority ordering is honored.

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
