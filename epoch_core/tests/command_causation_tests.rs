//! Tests for command causation tracking.

use epoch_core::prelude::*;
use uuid::Uuid;

#[derive(Debug, Clone)]
enum TestCommand {
    DoSomething,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
enum TestEvent {
    SomethingDone,
}

impl EventData for TestEvent {
    fn event_type(&self) -> &'static str {
        "SomethingDone"
    }
}

#[test]
fn command_new_defaults_causation_to_none() {
    let cmd = Command::<TestCommand, ()>::new(Uuid::new_v4(), TestCommand::DoSomething, None, None);

    assert_eq!(cmd.causation_id, None);
    assert_eq!(cmd.correlation_id, None);
}

#[test]
fn command_caused_by_sets_causation_and_correlation() {
    let correlation_id = Uuid::new_v4();
    let event_id = Uuid::new_v4();

    let event = Event::<TestEvent> {
        id: event_id,
        stream_id: Uuid::new_v4(),
        stream_version: 1,
        event_type: "SomethingDone".to_string(),
        actor_id: None,
        purger_id: None,
        data: Some(TestEvent::SomethingDone),
        created_at: chrono::Utc::now(),
        purged_at: None,
        global_sequence: Some(10),
        correlation_id: Some(correlation_id),
        causation_id: None,
    };

    let cmd = Command::<TestCommand, ()>::new(Uuid::new_v4(), TestCommand::DoSomething, None, None)
        .caused_by(&event);

    assert_eq!(cmd.causation_id, Some(event_id));
    assert_eq!(cmd.correlation_id, Some(correlation_id));
}

#[test]
fn command_caused_by_handles_event_without_correlation() {
    let event_id = Uuid::new_v4();

    let event = Event::<TestEvent> {
        id: event_id,
        stream_id: Uuid::new_v4(),
        stream_version: 1,
        event_type: "SomethingDone".to_string(),
        actor_id: None,
        purger_id: None,
        data: Some(TestEvent::SomethingDone),
        created_at: chrono::Utc::now(),
        purged_at: None,
        global_sequence: Some(10),
        correlation_id: None,
        causation_id: None,
    };

    let cmd = Command::<TestCommand, ()>::new(Uuid::new_v4(), TestCommand::DoSomething, None, None)
        .caused_by(&event);

    assert_eq!(cmd.causation_id, Some(event_id));
    assert_eq!(cmd.correlation_id, None);
}

#[test]
fn command_with_correlation_id_sets_explicit_correlation() {
    let correlation_id = Uuid::new_v4();

    let cmd = Command::<TestCommand, ()>::new(Uuid::new_v4(), TestCommand::DoSomething, None, None)
        .with_correlation_id(correlation_id);

    assert_eq!(cmd.correlation_id, Some(correlation_id));
    assert_eq!(cmd.causation_id, None);
}

#[test]
fn command_to_subset_preserves_causation() {
    let correlation_id = Uuid::new_v4();
    let causation_id = Uuid::new_v4();

    let mut cmd =
        Command::<TestCommand, ()>::new(Uuid::new_v4(), TestCommand::DoSomething, None, None);

    // Manually set causation fields for test
    cmd.causation_id = Some(causation_id);
    cmd.correlation_id = Some(correlation_id);

    // For this test, we need identity conversion
    let subset = cmd.to_subset_command::<TestCommand>().unwrap();

    assert_eq!(subset.correlation_id, Some(correlation_id));
    assert_eq!(subset.causation_id, Some(causation_id));
}
