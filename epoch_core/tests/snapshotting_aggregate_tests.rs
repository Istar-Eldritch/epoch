//! Integration tests for the `SnapshottingAggregate` extension trait.
//!
//! These exercise the opt-in capture/prune lifecycle (`capture_snapshot_if_due`
//! wired through `Aggregate::after_persist`) and the manual `save_snapshot` path
//! against in-memory backends.

use async_trait::async_trait;
use epoch_core::event::EnumConversionError;
use epoch_core::prelude::*;
use epoch_mem::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum CounterEvent {
    Created,
    Incremented,
}

impl EventData for CounterEvent {
    fn event_type(&self) -> &'static str {
        match self {
            CounterEvent::Created => "CounterCreated",
            CounterEvent::Incremented => "CounterIncremented",
        }
    }
}

impl TryFrom<&CounterEvent> for CounterEvent {
    type Error = EnumConversionError;

    fn try_from(value: &CounterEvent) -> Result<Self, Self::Error> {
        Ok(value.clone())
    }
}

#[derive(Debug, Clone)]
enum CounterCommand {
    Create,
    Increment,
    IncrementN(u64),
}

#[derive(Debug, Clone, PartialEq)]
struct Counter {
    id: Uuid,
    value: u64,
    version: u64,
}

impl EventApplicatorState for Counter {
    fn get_id(&self) -> &Uuid {
        &self.id
    }
}

impl AggregateState for Counter {
    fn get_version(&self) -> u64 {
        self.version
    }
    fn set_version(&mut self, version: u64) {
        self.version = version;
    }
}

struct CounterAggregate {
    state_store: InMemoryStateStore<Counter>,
    event_store: InMemoryEventStore<InMemoryEventBus<CounterEvent>>,
    snapshot_store: InMemorySnapshotStore<Counter>,
    config: SnapshotConfig,
}

#[derive(Debug, thiserror::Error)]
enum CounterApplyError {
    #[error("No state present for id {0}")]
    NoState(Uuid),
}

impl EventApplicator<CounterEvent> for CounterAggregate {
    type State = Counter;
    type EventType = CounterEvent;
    type StateStore = InMemoryStateStore<Counter>;
    type ApplyError = CounterApplyError;

    fn get_state_store(&self) -> Self::StateStore {
        self.state_store.clone()
    }

    fn apply(
        &self,
        state: Option<Self::State>,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, CounterApplyError> {
        match event.data.as_ref().unwrap() {
            CounterEvent::Created => Ok(Some(Counter {
                id: event.stream_id,
                value: 0,
                version: event.stream_version,
            })),
            CounterEvent::Incremented => {
                let mut state = state.ok_or(CounterApplyError::NoState(event.stream_id))?;
                state.value += 1;
                state.version = event.stream_version;
                Ok(Some(state))
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum CounterAggregateError {
    #[error("Error building event")]
    EventBuild(#[from] EventBuilderError),
}

#[async_trait]
impl Aggregate<CounterEvent> for CounterAggregate {
    type CommandData = CounterCommand;
    type CommandCredentials = ();
    type Command = CounterCommand;
    type AggregateError = CounterAggregateError;
    type EventStore = InMemoryEventStore<InMemoryEventBus<CounterEvent>>;

    fn get_event_store(&self) -> Self::EventStore {
        self.event_store.clone()
    }

    async fn handle_command(
        &self,
        _state: &Option<Self::State>,
        command: Command<Self::Command, Self::CommandCredentials>,
    ) -> Result<Vec<Event<CounterEvent>>, Self::AggregateError> {
        match command.data {
            CounterCommand::Create => Ok(vec![
                CounterEvent::Created
                    .into_builder()
                    .stream_id(command.aggregate_id)
                    .build()?,
            ]),
            CounterCommand::Increment => Ok(vec![
                CounterEvent::Incremented
                    .into_builder()
                    .stream_id(command.aggregate_id)
                    .build()?,
            ]),
            CounterCommand::IncrementN(n) => (0..n)
                .map(|_| {
                    CounterEvent::Incremented
                        .into_builder()
                        .stream_id(command.aggregate_id)
                        .build()
                        .map_err(CounterAggregateError::EventBuild)
                })
                .collect(),
        }
    }

    async fn after_persist(
        &self,
        stream_id: Uuid,
        new_version: u64,
        events_applied: usize,
        state: &Counter,
    ) {
        self.capture_snapshot_if_due(stream_id, new_version, events_applied, state)
            .await;
    }
}

#[async_trait]
impl SnapshottingAggregate<CounterEvent> for CounterAggregate {
    type SnapshotStore = InMemorySnapshotStore<Counter>;

    fn snapshot_store(&self) -> Self::SnapshotStore {
        self.snapshot_store.clone()
    }

    fn snapshot_config(&self) -> &SnapshotConfig {
        &self.config
    }
}

fn build_aggregate(config: SnapshotConfig) -> (CounterAggregate, InMemorySnapshotStore<Counter>) {
    let bus = InMemoryEventBus::<CounterEvent>::new();
    let event_store = InMemoryEventStore::new(bus);
    let state_store = InMemoryStateStore::new();
    let snapshot_store = InMemorySnapshotStore::<Counter>::new();
    let aggregate = CounterAggregate {
        state_store,
        event_store,
        snapshot_store: snapshot_store.clone(),
        config,
    };
    (aggregate, snapshot_store)
}

async fn drive(aggregate: &CounterAggregate, id: Uuid, commands: u64) {
    aggregate
        .handle(Command::new(id, CounterCommand::Create, None, None))
        .await
        .unwrap();
    for _ in 1..commands {
        aggregate
            .handle(Command::new(id, CounterCommand::Increment, None, None))
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn automatic_capture_honours_interval() {
    let (aggregate, snapshots) = build_aggregate(SnapshotConfig {
        trigger: SnapshotTrigger::Automatic { interval: 3 },
        retention: SnapshotRetention::Unlimited,
    });
    let id = Uuid::new_v4();

    // 7 commands -> versions 1..=7. Boundaries crossed at 3 and 6.
    drive(&aggregate, id, 7).await;

    let snap_latest = snapshots.load_snapshot(id, 100).await.unwrap().unwrap();
    assert_eq!(snap_latest.version, 6);
    assert_eq!(snap_latest.state.value, 5); // Created(v1)=0 + 5 Incremented = 5

    let snap_3 = snapshots.load_snapshot(id, 3).await.unwrap().unwrap();
    assert_eq!(snap_3.version, 3);
    assert_eq!(snap_3.state.value, 2); // Created(v1)=0 + 2 Incremented = 2

    assert!(snapshots.load_snapshot(id, 2).await.unwrap().is_none());
}

#[tokio::test]
async fn keep_last_prunes_to_n_highest() {
    let (aggregate, snapshots) = build_aggregate(SnapshotConfig {
        trigger: SnapshotTrigger::Automatic { interval: 3 },
        retention: SnapshotRetention::KeepLast(2),
    });
    let id = Uuid::new_v4();

    // 10 commands -> versions 1..=10. Snapshots at 3, 6, 9; KeepLast(2) leaves 6 and 9.
    drive(&aggregate, id, 10).await;

    assert!(snapshots.load_snapshot(id, 5).await.unwrap().is_none());

    let snap_6 = snapshots.load_snapshot(id, 6).await.unwrap().unwrap();
    assert_eq!(snap_6.version, 6);
    assert_eq!(snap_6.state.value, 5); // Created(v1)=0 + 5 Incremented = 5

    let snap_9 = snapshots.load_snapshot(id, 100).await.unwrap().unwrap();
    assert_eq!(snap_9.version, 9);
    assert_eq!(snap_9.state.value, 8); // Created(v1)=0 + 8 Incremented = 8
}

#[tokio::test]
async fn manual_trigger_never_auto_captures() {
    let (aggregate, snapshots) = build_aggregate(SnapshotConfig {
        trigger: SnapshotTrigger::Manual,
        retention: SnapshotRetention::Unlimited,
    });
    let id = Uuid::new_v4();

    drive(&aggregate, id, 10).await;

    assert!(snapshots.load_snapshot(id, 100).await.unwrap().is_none());
}

#[tokio::test]
async fn interval_zero_never_captures() {
    let (aggregate, snapshots) = build_aggregate(SnapshotConfig {
        trigger: SnapshotTrigger::Automatic { interval: 0 },
        retention: SnapshotRetention::Unlimited,
    });
    let id = Uuid::new_v4();

    drive(&aggregate, id, 10).await;

    assert!(snapshots.load_snapshot(id, 100).await.unwrap().is_none());
}

#[tokio::test]
async fn manual_save_snapshot_persists_current_state() {
    let (aggregate, snapshots) = build_aggregate(SnapshotConfig {
        trigger: SnapshotTrigger::Manual,
        retention: SnapshotRetention::Unlimited,
    });
    let id = Uuid::new_v4();

    drive(&aggregate, id, 4).await; // versions 1..=4, value 3, no auto-capture

    aggregate.save_snapshot(id).await.unwrap();

    let snap = snapshots.load_snapshot(id, 100).await.unwrap().unwrap();
    assert_eq!(snap.version, 4);
    assert_eq!(snap.state.value, 3);
}

#[tokio::test]
async fn manual_save_snapshot_errors_without_state() {
    let (aggregate, _snapshots) = build_aggregate(SnapshotConfig {
        trigger: SnapshotTrigger::Manual,
        retention: SnapshotRetention::Unlimited,
    });

    let err = aggregate.save_snapshot(Uuid::new_v4()).await.unwrap_err();
    assert!(matches!(err, SaveSnapshotError::NoState));
}

// ── FIX 5: multi-event command boundary-crossing tests ───────────────────────

#[tokio::test]
async fn multi_event_command_spanning_boundary() {
    // Create(v1)+Inc(v2)+Inc(v3)+IncrementN(2)→v4,v5; prev=3, new=5, interval=5: capture at v5.
    let (aggregate, snapshots) = build_aggregate(SnapshotConfig {
        trigger: SnapshotTrigger::Automatic { interval: 5 },
        retention: SnapshotRetention::Unlimited,
    });
    let id = Uuid::new_v4();

    aggregate
        .handle(Command::new(id, CounterCommand::Create, None, None))
        .await
        .unwrap();
    aggregate
        .handle(Command::new(id, CounterCommand::Increment, None, None))
        .await
        .unwrap();
    aggregate
        .handle(Command::new(id, CounterCommand::Increment, None, None))
        .await
        .unwrap();
    // n=2 is deliberate: n>=3 skips a version in handle()'s stamping loop (CLOUD-185).
    aggregate
        .handle(Command::new(id, CounterCommand::IncrementN(2), None, None))
        .await
        .unwrap();

    // Exactly one snapshot at v5 (new_version), state.value = 4
    let snap = snapshots.load_snapshot(id, 100).await.unwrap().unwrap();
    assert_eq!(snap.version, 5);
    assert_eq!(snap.state.value, 4); // Created=0 + 4 Incremented
    assert!(snapshots.load_snapshot(id, 4).await.unwrap().is_none());
}

#[tokio::test]
async fn multi_event_command_skipping_boundary() {
    // Create(v1)+Inc(v2)+Inc(v3)+Inc(v4)+IncrementN(2)→v5,v6; prev=4, new=6, interval=5:
    // boundary at v5 in (4,6] but new_version=6 is not a multiple, capture at v6.
    let (aggregate, snapshots) = build_aggregate(SnapshotConfig {
        trigger: SnapshotTrigger::Automatic { interval: 5 },
        retention: SnapshotRetention::Unlimited,
    });
    let id = Uuid::new_v4();

    aggregate
        .handle(Command::new(id, CounterCommand::Create, None, None))
        .await
        .unwrap();
    for _ in 0..3 {
        aggregate
            .handle(Command::new(id, CounterCommand::Increment, None, None))
            .await
            .unwrap();
    }
    // n=2 is deliberate: n>=3 skips a version in handle()'s stamping loop (CLOUD-185).
    aggregate
        .handle(Command::new(id, CounterCommand::IncrementN(2), None, None))
        .await
        .unwrap();

    // Exactly one snapshot at v6 (new_version), state.value = 5
    let snap = snapshots.load_snapshot(id, 100).await.unwrap().unwrap();
    assert_eq!(snap.version, 6);
    assert_eq!(snap.state.value, 5); // Created=0 + 5 Incremented
    // No snapshot at v5 even though the boundary was crossed there
    assert!(snapshots.load_snapshot(id, 5).await.unwrap().is_none());
}

// ── FIX 6: snapshot store failure swallowing tests ───────────────────────────

#[derive(Debug, thiserror::Error)]
#[error("mock snapshot store error")]
struct MockSnapshotError;

#[derive(Clone)]
struct FailingSaveSnapshotStore {
    save_calls: Arc<AtomicUsize>,
}

impl FailingSaveSnapshotStore {
    fn new() -> Self {
        Self {
            save_calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl SnapshotStore<Counter> for FailingSaveSnapshotStore {
    type Error = MockSnapshotError;

    async fn load_snapshot(
        &self,
        _: Uuid,
        _: u64,
    ) -> Result<Option<Snapshot<Counter>>, Self::Error> {
        Ok(None)
    }

    async fn save_snapshot(&self, _: Uuid, _: u64, _: &Counter) -> Result<(), Self::Error> {
        self.save_calls.fetch_add(1, Ordering::SeqCst);
        Err(MockSnapshotError)
    }

    async fn apply_retention(&self, _: Uuid, _: &SnapshotRetention) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[derive(Clone)]
struct FailingRetentionSnapshotStore {
    inner: InMemorySnapshotStore<Counter>,
}

#[async_trait]
impl SnapshotStore<Counter> for FailingRetentionSnapshotStore {
    type Error = MockSnapshotError;

    async fn load_snapshot(
        &self,
        id: Uuid,
        v: u64,
    ) -> Result<Option<Snapshot<Counter>>, Self::Error> {
        self.inner
            .load_snapshot(id, v)
            .await
            .map_err(|e| match e {})
    }

    async fn save_snapshot(&self, id: Uuid, v: u64, state: &Counter) -> Result<(), Self::Error> {
        self.inner
            .save_snapshot(id, v, state)
            .await
            .map_err(|e| match e {})
    }

    async fn apply_retention(&self, _: Uuid, _: &SnapshotRetention) -> Result<(), Self::Error> {
        Err(MockSnapshotError)
    }
}

struct CounterAggregateCustomStore<SS> {
    state_store: InMemoryStateStore<Counter>,
    event_store: InMemoryEventStore<InMemoryEventBus<CounterEvent>>,
    snapshot_store: SS,
    config: SnapshotConfig,
}

impl<SS> EventApplicator<CounterEvent> for CounterAggregateCustomStore<SS>
where
    SS: Send + Sync,
{
    type State = Counter;
    type EventType = CounterEvent;
    type StateStore = InMemoryStateStore<Counter>;
    type ApplyError = CounterApplyError;

    fn get_state_store(&self) -> Self::StateStore {
        self.state_store.clone()
    }

    fn apply(
        &self,
        state: Option<Self::State>,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, CounterApplyError> {
        match event.data.as_ref().unwrap() {
            CounterEvent::Created => Ok(Some(Counter {
                id: event.stream_id,
                value: 0,
                version: event.stream_version,
            })),
            CounterEvent::Incremented => {
                let mut s = state.ok_or(CounterApplyError::NoState(event.stream_id))?;
                s.value += 1;
                s.version = event.stream_version;
                Ok(Some(s))
            }
        }
    }
}

#[async_trait]
impl<SS> Aggregate<CounterEvent> for CounterAggregateCustomStore<SS>
where
    SS: SnapshotStore<Counter> + Send + Sync + Clone + 'static,
{
    type CommandData = CounterCommand;
    type CommandCredentials = ();
    type Command = CounterCommand;
    type AggregateError = CounterAggregateError;
    type EventStore = InMemoryEventStore<InMemoryEventBus<CounterEvent>>;

    fn get_event_store(&self) -> Self::EventStore {
        self.event_store.clone()
    }

    async fn handle_command(
        &self,
        _state: &Option<Self::State>,
        command: Command<Self::Command, Self::CommandCredentials>,
    ) -> Result<Vec<Event<CounterEvent>>, Self::AggregateError> {
        match command.data {
            CounterCommand::Create => Ok(vec![
                CounterEvent::Created
                    .into_builder()
                    .stream_id(command.aggregate_id)
                    .build()?,
            ]),
            CounterCommand::Increment => Ok(vec![
                CounterEvent::Incremented
                    .into_builder()
                    .stream_id(command.aggregate_id)
                    .build()?,
            ]),
            CounterCommand::IncrementN(n) => (0..n)
                .map(|_| {
                    CounterEvent::Incremented
                        .into_builder()
                        .stream_id(command.aggregate_id)
                        .build()
                        .map_err(CounterAggregateError::EventBuild)
                })
                .collect(),
        }
    }

    async fn after_persist(
        &self,
        stream_id: Uuid,
        new_version: u64,
        events_applied: usize,
        state: &Counter,
    ) {
        self.capture_snapshot_if_due(stream_id, new_version, events_applied, state)
            .await;
    }
}

#[async_trait]
impl<SS> SnapshottingAggregate<CounterEvent> for CounterAggregateCustomStore<SS>
where
    SS: SnapshotStore<Counter> + Send + Sync + Clone + 'static,
{
    type SnapshotStore = SS;

    fn snapshot_store(&self) -> Self::SnapshotStore {
        self.snapshot_store.clone()
    }

    fn snapshot_config(&self) -> &SnapshotConfig {
        &self.config
    }
}

#[tokio::test]
async fn snapshot_save_failure_does_not_fail_command() {
    let id = Uuid::new_v4();
    let bus = InMemoryEventBus::<CounterEvent>::new();
    let failing_store = FailingSaveSnapshotStore::new();
    let save_calls = Arc::clone(&failing_store.save_calls);
    let aggregate = CounterAggregateCustomStore {
        state_store: InMemoryStateStore::new(),
        event_store: InMemoryEventStore::new(bus),
        snapshot_store: failing_store,
        config: SnapshotConfig {
            trigger: SnapshotTrigger::Automatic { interval: 3 },
            retention: SnapshotRetention::Unlimited,
        },
    };

    // Drive to v3: snapshot capture triggered; save_snapshot always returns Err.
    aggregate
        .handle(Command::new(id, CounterCommand::Create, None, None))
        .await
        .unwrap();
    aggregate
        .handle(Command::new(id, CounterCommand::Increment, None, None))
        .await
        .unwrap();
    let result = aggregate
        .handle(Command::new(id, CounterCommand::Increment, None, None))
        .await;

    assert!(
        result.is_ok(),
        "command must not fail due to snapshot save error"
    );
    // Proves the failing save path was actually exercised (not a vacuous pass).
    assert!(
        save_calls.load(Ordering::SeqCst) >= 1,
        "save_snapshot must have been called at least once"
    );
    // Documenting the mock's stub-load behavior; not a meaningful storage assertion.
    let snap = aggregate
        .snapshot_store
        .load_snapshot(id, 100)
        .await
        .unwrap();
    assert!(
        snap.is_none(),
        "no snapshot should be stored when save fails"
    );
}

#[tokio::test]
async fn snapshot_retention_failure_does_not_fail_command() {
    let id = Uuid::new_v4();
    let inner = InMemorySnapshotStore::<Counter>::new();
    let bus = InMemoryEventBus::<CounterEvent>::new();
    let aggregate = CounterAggregateCustomStore {
        state_store: InMemoryStateStore::new(),
        event_store: InMemoryEventStore::new(bus),
        snapshot_store: FailingRetentionSnapshotStore {
            inner: inner.clone(),
        },
        config: SnapshotConfig {
            trigger: SnapshotTrigger::Automatic { interval: 3 },
            retention: SnapshotRetention::Unlimited,
        },
    };

    // Drive to v3: save succeeds, apply_retention always returns Err.
    aggregate
        .handle(Command::new(id, CounterCommand::Create, None, None))
        .await
        .unwrap();
    aggregate
        .handle(Command::new(id, CounterCommand::Increment, None, None))
        .await
        .unwrap();
    let result = aggregate
        .handle(Command::new(id, CounterCommand::Increment, None, None))
        .await;

    assert!(
        result.is_ok(),
        "command must not fail due to retention error"
    );
    // Snapshot was persisted even though retention failed.
    let snap = inner.load_snapshot(id, 100).await.unwrap();
    assert!(
        snap.is_some(),
        "snapshot must be persisted when only retention fails"
    );
    assert_eq!(snap.unwrap().version, 3);
}
