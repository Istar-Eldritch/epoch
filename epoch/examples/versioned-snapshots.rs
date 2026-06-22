//! Versioned snapshot store example.
//!
//! Demonstrates:
//!   - Automatic snapshot capture via [`SnapshottingAggregate`] and [`Aggregate::after_persist`]
//!   - Retention policy (keep the last N snapshots per stream)
//!   - Historical state reconstruction with [`state_at`]
//!   - Manual snapshot via [`SnapshottingAggregate::save_snapshot`]
//!
//! Run with:
//!   `cargo run --example versioned-snapshots`

use async_trait::async_trait;
use epoch::prelude::*;
use epoch_mem::{InMemoryEventBus, InMemoryEventStore, InMemorySnapshotStore, InMemoryStateStore};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── Events ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, EventData)]
pub enum AccountEvent {
    Deposited { amount: u64 },
    Withdrawn { amount: u64 },
}

// When the aggregate and applicator use the same type as both the superset and
// the subset event type, we implement the narrowing conversion by cloning.
impl TryFrom<&AccountEvent> for AccountEvent {
    type Error = EnumConversionError;
    fn try_from(v: &AccountEvent) -> Result<Self, Self::Error> {
        Ok(v.clone())
    }
}

// ── Commands ──────────────────────────────────────────────────────────────────

#[subset_enum(AccountCommand, Deposit, Withdraw)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AppCommand {
    Deposit { amount: u64 },
    Withdraw { amount: u64 },
}

// ── State ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountState {
    pub id: Uuid,
    pub balance: u64,
    pub version: u64,
}

impl EventApplicatorState for AccountState {
    fn get_id(&self) -> &Uuid {
        &self.id
    }
}

impl AggregateState for AccountState {
    fn get_version(&self) -> u64 {
        self.version
    }
    fn set_version(&mut self, version: u64) {
        self.version = version;
    }
}

// ── Errors ────────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum AccountApplyError {}

#[derive(Debug, thiserror::Error)]
pub enum AccountCommandError {
    #[error("Insufficient funds: balance {balance}, requested {amount}")]
    InsufficientFunds { balance: u64, amount: u64 },
    #[error("Account does not exist")]
    NoAccount,
}

// ── Aggregate ─────────────────────────────────────────────────────────────────

type AccountEventStore = InMemoryEventStore<InMemoryEventBus<AccountEvent>>;
type AccountStateStore = InMemoryStateStore<AccountState>;
type AccountSnapshotStore = InMemorySnapshotStore<AccountState>;

/// A bank account aggregate that automatically captures a versioned snapshot
/// every 5 events and retains the 2 most recent snapshots.
pub struct BankAccount {
    event_store: AccountEventStore,
    state_store: AccountStateStore,
    snapshot_store: AccountSnapshotStore,
    snapshot_config: SnapshotConfig,
}

impl BankAccount {
    pub fn new(
        event_store: AccountEventStore,
        state_store: AccountStateStore,
        snapshot_store: AccountSnapshotStore,
    ) -> Self {
        Self {
            event_store,
            state_store,
            snapshot_store,
            snapshot_config: SnapshotConfig {
                trigger: SnapshotTrigger::Automatic { interval: 5 },
                retention: SnapshotRetention::KeepLast(2),
            },
        }
    }
}

impl EventApplicator<AccountEvent> for BankAccount {
    type State = AccountState;
    type StateStore = AccountStateStore;
    type EventType = AccountEvent;
    type ApplyError = AccountApplyError;

    fn get_state_store(&self) -> Self::StateStore {
        self.state_store.clone()
    }

    fn apply(
        &self,
        state: Option<Self::State>,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::ApplyError> {
        let mut acc = state.unwrap_or(AccountState {
            id: event.stream_id,
            balance: 0,
            version: 0,
        });
        acc.version = event.stream_version;
        match event.data.as_ref().unwrap() {
            AccountEvent::Deposited { amount } => acc.balance += amount,
            AccountEvent::Withdrawn { amount } => acc.balance -= amount,
        }
        Ok(Some(acc))
    }
}

#[async_trait]
impl Aggregate<AccountEvent> for BankAccount {
    type CommandData = AppCommand;
    type CommandCredentials = ();
    type Command = AccountCommand;
    type EventStore = AccountEventStore;
    type AggregateError = AccountCommandError;

    fn get_event_store(&self) -> Self::EventStore {
        self.event_store.clone()
    }

    async fn handle_command(
        &self,
        state: &Option<Self::State>,
        command: Command<Self::Command, ()>,
    ) -> Result<Vec<Event<AccountEvent>>, Self::AggregateError> {
        match command.data {
            AccountCommand::Deposit { amount } => Ok(vec![
                AccountEvent::Deposited { amount }
                    .into_builder()
                    .stream_id(command.aggregate_id)
                    .build()
                    .expect("valid event builder"),
            ]),
            AccountCommand::Withdraw { amount } => {
                let balance = state
                    .as_ref()
                    .ok_or(AccountCommandError::NoAccount)?
                    .balance;
                if amount > balance {
                    return Err(AccountCommandError::InsufficientFunds { balance, amount });
                }
                Ok(vec![
                    AccountEvent::Withdrawn { amount }
                        .into_builder()
                        .stream_id(command.aggregate_id)
                        .build()
                        .expect("valid event builder"),
                ])
            }
        }
    }

    // Wire the lifecycle hook to automatic snapshot capture. Store failures are
    // logged and swallowed — a snapshot is a rebuildable cache, never a blocker.
    async fn after_persist(
        &self,
        stream_id: Uuid,
        new_version: u64,
        events_applied: usize,
        state: &AccountState,
    ) {
        self.capture_snapshot_if_due(stream_id, new_version, events_applied, state)
            .await;
    }
}

impl SnapshottingAggregate<AccountEvent> for BankAccount {
    type SnapshotStore = AccountSnapshotStore;

    fn snapshot_store(&self) -> Self::SnapshotStore {
        self.snapshot_store.clone()
    }

    fn snapshot_config(&self) -> &SnapshotConfig {
        &self.snapshot_config
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let account_id = Uuid::new_v4();

    let bus = InMemoryEventBus::<AccountEvent>::new();
    let event_store = InMemoryEventStore::new(bus);
    let state_store: AccountStateStore = InMemoryStateStore::new();
    let snapshot_store: AccountSnapshotStore = InMemorySnapshotStore::new();

    let aggregate = BankAccount::new(
        event_store.clone(),
        state_store.clone(),
        snapshot_store.clone(),
    );

    // Issue 12 deposits of $100. With interval=5 and KeepLast(2), automatic
    // snapshots fire at versions 5 and 10 (v5 is pruned after v10 is taken,
    // leaving only v10 in the store — KeepLast(2) keeps the two newest).
    //
    // But wait: v5 is still around when v10 fires because KeepLast applies
    // AFTER the new snapshot is written. After v10 fires: store holds [v5, v10].
    // After v15 fires (if we had 15 events): store would hold [v10, v15].
    println!("--- Issuing 12 deposits ---");
    for i in 1u64..=12 {
        aggregate
            .handle(Command::new(
                account_id,
                AppCommand::Deposit { amount: 100 },
                None,
                None,
            ))
            .await?;
        if i == 5 || i == 10 {
            println!("  → automatic snapshot captured at version {i}");
        }
    }

    let final_state = state_store
        .get_state(account_id)
        .await?
        .expect("account must exist after deposits");
    println!(
        "\nFinal state: balance=${}, version={}",
        final_state.balance, final_state.version
    );
    assert_eq!(final_state.balance, 1200, "12 × $100 = $1200");
    assert_eq!(final_state.version, 12);

    // ── state_at: historical reconstruction ───────────────────────────────────
    //
    // state_at(v7) uses the snapshot at v5 as a starting point, then replays
    // only events v6 and v7 — much cheaper than a full replay from v1.
    println!("\n--- Reconstructing state at version 7 ---");
    let state_v7 = state_at(&aggregate, &event_store, &snapshot_store, account_id, 7)
        .await?
        .expect("stream must have events up to version 7");

    println!(
        "Balance at v7: ${} (snapshot@v5 + 2 events replayed)",
        state_v7.balance
    );
    assert_eq!(
        state_v7.balance, 700,
        "7 deposits × $100 = $700"
    );

    // ── Manual snapshot ───────────────────────────────────────────────────────
    //
    // save_snapshot reads the current live state and persists it as a versioned
    // snapshot at the state's current version (12 in this case).
    println!("\n--- Saving manual snapshot ---");
    aggregate.save_snapshot(account_id).await?;
    println!("Manual snapshot saved at version {}", final_state.version);

    println!("\nAll assertions passed!");
    Ok(())
}
