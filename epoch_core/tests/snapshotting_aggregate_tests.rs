//! Integration tests for the `SnapshottingAggregate` extension trait.
//!
//! These exercise the opt-in capture/prune lifecycle (`capture_snapshot_if_due`
//! wired through `Aggregate::after_persist`) and the manual `save_snapshot` path
//! against in-memory backends.

use async_trait::async_trait;
use epoch_core::event::EnumConversionError;
use epoch_core::prelude::*;
use epoch_mem::*;
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
        let event = match command.data {
            CounterCommand::Create => CounterEvent::Created,
            CounterCommand::Increment => CounterEvent::Incremented,
        };
        Ok(vec![
            event
                .into_builder()
                .stream_id(command.aggregate_id)
                .build()?,
        ])
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

    assert_eq!(
        snapshots
            .load_snapshot(id, 100)
            .await
            .unwrap()
            .map(|s| s.version),
        Some(6)
    );
    assert_eq!(
        snapshots
            .load_snapshot(id, 3)
            .await
            .unwrap()
            .map(|s| s.version),
        Some(3)
    );
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
    assert_eq!(
        snapshots
            .load_snapshot(id, 6)
            .await
            .unwrap()
            .map(|s| s.version),
        Some(6)
    );
    assert_eq!(
        snapshots
            .load_snapshot(id, 100)
            .await
            .unwrap()
            .map(|s| s.version),
        Some(9)
    );
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
