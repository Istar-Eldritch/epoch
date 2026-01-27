//! # Transactional Aggregate Example
//!
//! This example demonstrates the `TransactionalAggregate` trait for atomic command handling.
//! It shows a bank account transfer scenario where money must be debited from one account
//! and credited to another atomically.
//!
//! ## Key Concepts Demonstrated
//!
//! 1. **Atomic Operations**: Multiple commands in a single transaction
//! 2. **Rollback on Error**: Failed operations don't affect state
//! 3. **Optimistic Concurrency**: Version checking prevents conflicts
//! 4. **Version Continuity**: Multiple handles on same aggregate within a transaction
//! 5. **Read-Your-Writes**: State changes visible within the transaction
//!
//! ## When to Use Transactions
//!
//! Use transactions when you need:
//! - Multiple commands that must succeed or fail together
//! - Atomicity across multiple aggregates
//! - Batch operations with single commit (performance)
//!
//! Use regular `handle()` for:
//! - Single command operations
//! - When eventual consistency is acceptable
//! - Maximum throughput (no transaction overhead)

use async_trait::async_trait;
use epoch::prelude::*;
use epoch_derive::subset_enum;
use epoch_mem::{
    impl_inmemory_transactional_aggregate, InMemoryEventBus, InMemoryEventStore,
    InMemoryStateStore,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

// ============================================================================
// Domain Model
// ============================================================================

/// Application-wide event enum containing all event types
#[subset_enum(AccountEvent, AccountCreated, AccountDebited, AccountCredited)]
#[derive(Debug, Clone, Serialize, Deserialize, EventData)]
pub enum ApplicationEvent {
    /// An account was created with an initial balance
    AccountCreated { initial_balance: i64 },
    /// Money was debited from an account
    AccountDebited { amount: i64, new_balance: i64 },
    /// Money was credited to an account
    AccountCredited { amount: i64, new_balance: i64 },
}

/// Application-wide command enum containing all command types
#[subset_enum(AccountCommand, CreateAccount, DebitAccount, CreditAccount)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ApplicationCommand {
    /// Create a new account with an initial balance
    CreateAccount { initial_balance: i64 },
    /// Debit (withdraw) money from an account
    DebitAccount { amount: i64 },
    /// Credit (deposit) money to an account
    CreditAccount { amount: i64 },
}

/// The state of a bank account
#[derive(Debug, Clone, Serialize)]
pub struct Account {
    id: Uuid,
    balance: i64,
    version: u64,
}

impl EventApplicatorState for Account {
    fn get_id(&self) -> &Uuid {
        &self.id
    }
}

impl AggregateState for Account {
    fn get_version(&self) -> u64 {
        self.version
    }

    fn set_version(&mut self, version: u64) {
        self.version = version;
    }
}

// ============================================================================
// Error Types
// ============================================================================

/// Errors that can occur when handling account commands
#[derive(Debug, thiserror::Error)]
pub enum AccountError {
    #[error("Insufficient funds: balance={balance}, attempted debit={amount}")]
    InsufficientFunds { balance: i64, amount: i64 },

    #[error("Invalid amount: {0}")]
    InvalidAmount(i64),

    #[error("Account already exists")]
    AccountAlreadyExists,

    #[error("Account does not exist")]
    AccountDoesNotExist,

    #[error("Event builder error: {0}")]
    EventBuilder(#[from] EventBuilderError),
}

/// Errors that can occur when applying events to account state
#[derive(Debug, thiserror::Error)]
pub enum AccountApplyError {
    #[error("Cannot apply event to non-existent account")]
    AccountDoesNotExist,
}

// ============================================================================
// Aggregate Implementation
// ============================================================================

/// Bank account aggregate that handles account commands
pub struct AccountAggregate {
    /// Lock for transaction isolation (in-memory only)
    lock: Arc<Mutex<()>>,
    /// Event store for persisting events
    event_store: InMemoryEventStore<InMemoryEventBus<ApplicationEvent>>,
    /// State store for persisting account state
    state_store: InMemoryStateStore<Account>,
}

impl AccountAggregate {
    /// Creates a new account aggregate
    pub fn new(
        lock: Arc<Mutex<()>>,
        event_store: InMemoryEventStore<InMemoryEventBus<ApplicationEvent>>,
        state_store: InMemoryStateStore<Account>,
    ) -> Self {
        AccountAggregate {
            lock,
            event_store,
            state_store,
        }
    }
}

impl EventApplicator<ApplicationEvent> for AccountAggregate {
    type State = Account;
    type EventType = AccountEvent;
    type StateStore = InMemoryStateStore<Account>;
    type ApplyError = AccountApplyError;

    fn get_state_store(&self) -> Self::StateStore {
        self.state_store.clone()
    }

    /// Applies an event to update account state
    fn apply(
        &self,
        state: Option<Self::State>,
        event: &Event<Self::EventType>,
    ) -> Result<Option<Self::State>, Self::ApplyError> {
        match event.data.as_ref().unwrap() {
            AccountEvent::AccountCreated { initial_balance } => Ok(Some(Account {
                id: event.stream_id,
                balance: *initial_balance,
                version: 0,
            })),
            AccountEvent::AccountDebited {
                amount: _,
                new_balance,
            } => {
                if let Some(mut account) = state {
                    account.balance = *new_balance;
                    Ok(Some(account))
                } else {
                    Err(AccountApplyError::AccountDoesNotExist)
                }
            }
            AccountEvent::AccountCredited {
                amount: _,
                new_balance,
            } => {
                if let Some(mut account) = state {
                    account.balance = *new_balance;
                    Ok(Some(account))
                } else {
                    Err(AccountApplyError::AccountDoesNotExist)
                }
            }
        }
    }
}

#[async_trait]
impl Aggregate<ApplicationEvent> for AccountAggregate {
    type CommandData = ApplicationCommand;
    type CommandCredentials = ();
    type Command = AccountCommand;
    type AggregateError = AccountError;
    type EventStore = InMemoryEventStore<InMemoryEventBus<ApplicationEvent>>;

    fn get_event_store(&self) -> Self::EventStore {
        self.event_store.clone()
    }

    /// Handles a command and produces events with business logic validation
    async fn handle_command(
        &self,
        state: &Option<Self::State>,
        command: Command<Self::Command, Self::CommandCredentials>,
    ) -> Result<Vec<Event<ApplicationEvent>>, Self::AggregateError> {
        match command.data {
            AccountCommand::CreateAccount { initial_balance } => {
                // Validation: cannot create if already exists
                if state.is_some() {
                    return Err(AccountError::AccountAlreadyExists);
                }

                // Validation: initial balance cannot be negative
                if initial_balance < 0 {
                    return Err(AccountError::InvalidAmount(initial_balance));
                }

                let event = ApplicationEvent::AccountCreated { initial_balance }
                    .into_builder()
                    .stream_id(command.aggregate_id)
                    .build()?;
                Ok(vec![event])
            }

            AccountCommand::DebitAccount { amount } => {
                // Validation: account must exist
                let account = state.as_ref().ok_or(AccountError::AccountDoesNotExist)?;

                // Validation: amount must be positive
                if amount <= 0 {
                    return Err(AccountError::InvalidAmount(amount));
                }

                // Validation: sufficient funds
                if account.balance < amount {
                    return Err(AccountError::InsufficientFunds {
                        balance: account.balance,
                        amount,
                    });
                }

                let new_balance = account.balance - amount;
                let event = ApplicationEvent::AccountDebited { amount, new_balance }
                    .into_builder()
                    .stream_id(command.aggregate_id)
                    .build()?;
                Ok(vec![event])
            }

            AccountCommand::CreditAccount { amount } => {
                // Validation: account must exist
                let account = state.as_ref().ok_or(AccountError::AccountDoesNotExist)?;

                // Validation: amount must be positive
                if amount <= 0 {
                    return Err(AccountError::InvalidAmount(amount));
                }

                let new_balance = account.balance + amount;
                let event = ApplicationEvent::AccountCredited { amount, new_balance }
                    .into_builder()
                    .stream_id(command.aggregate_id)
                    .build()?;
                Ok(vec![event])
            }
        }
    }
}

// ============================================================================
// TransactionalAggregate Implementation (via macro)
// ============================================================================

impl_inmemory_transactional_aggregate! {
    aggregate: AccountAggregate,
    event: ApplicationEvent,
    bus: InMemoryEventBus<ApplicationEvent>,
    lock_field: lock,
    event_store_field: event_store,
    state_store_field: state_store,
}

// The macro above generates the full TransactionalAggregate implementation!
// No need to manually implement all ~50 lines of boilerplate.

// ============================================================================
// Helper Functions
// ============================================================================

/// Creates a new account with an initial balance
async fn create_account(
    aggregate: &AccountAggregate,
    initial_balance: i64,
) -> Result<(Uuid, Account), Box<dyn std::error::Error>> {
    let account_id = Uuid::new_v4();
    let command = Command::new(
        account_id,
        ApplicationCommand::CreateAccount { initial_balance },
        None,
        None,
    );

    let state = aggregate.handle(command).await?;
    Ok((
        account_id,
        state.expect("Account should exist after creation"),
    ))
}

/// Transfers money from one account to another atomically
///
/// Both the debit and credit operations are performed in a single transaction.
/// If either operation fails, both are rolled back.
async fn transfer_money(
    aggregate: Arc<AccountAggregate>,
    from_id: Uuid,
    to_id: Uuid,
    amount: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut tx = aggregate.clone().begin().await?;

    // Debit from source account
    tx.handle(Command::new(
        from_id,
        ApplicationCommand::DebitAccount { amount },
        None,
        None,
    ))
    .await?;

    // Credit to destination account
    tx.handle(Command::new(
        to_id,
        ApplicationCommand::CreditAccount { amount },
        None,
        None,
    ))
    .await?;

    // Commit atomically - both operations persist together
    tx.commit().await?;
    Ok(())
}

/// Gets the current state of an account
async fn get_account(
    aggregate: &AccountAggregate,
    id: Uuid,
) -> Result<Option<Account>, Box<dyn std::error::Error>> {
    let state_store = aggregate.get_state_store();
    Ok(state_store.get_state(id).await?)
}

// ============================================================================
// Main Example
// ============================================================================

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    println!("\n=== Transactional Aggregate Example ===\n");

    // Setup: Create event bus, stores, and aggregate
    let bus = InMemoryEventBus::new();
    let event_store = InMemoryEventStore::new(bus);
    let state_store = InMemoryStateStore::new();
    let lock = Arc::new(Mutex::new(()));

    let aggregate = AccountAggregate::new(lock, event_store, state_store);
    let aggregate = Arc::new(aggregate);

    // ========================================================================
    // Scenario 1: Create two accounts
    // ========================================================================
    println!("Creating accounts...");
    let (account_a_id, account_a) = create_account(&aggregate, 1000).await?;
    println!(
        "✓ Account A created: id={}, balance={}, version={}",
        account_a_id, account_a.balance, account_a.version
    );

    let (account_b_id, account_b) = create_account(&aggregate, 500).await?;
    println!(
        "✓ Account B created: id={}, balance={}, version={}",
        account_b_id, account_b.balance, account_b.version
    );
    println!();

    // ========================================================================
    // Scenario 2: Successful atomic transfer
    // ========================================================================
    println!("Transferring $300 from A to B atomically...");
    transfer_money(aggregate.clone(), account_a_id, account_b_id, 300).await?;

    let account_a = get_account(&aggregate, account_a_id).await?.unwrap();
    let account_b = get_account(&aggregate, account_b_id).await?.unwrap();

    println!("✓ Transfer succeeded");
    println!(
        "  Account A: balance={}, version={}",
        account_a.balance, account_a.version
    );
    println!(
        "  Account B: balance={}, version={}",
        account_b.balance, account_b.version
    );
    println!();

    // ========================================================================
    // Scenario 3: Failed transfer (insufficient funds) with rollback
    // ========================================================================
    println!("Attempting invalid transfer ($1000 from B to A)...");

    let result = transfer_money(aggregate.clone(), account_b_id, account_a_id, 1000).await;

    match result {
        Ok(_) => println!("✗ Transfer should have failed!"),
        Err(e) => {
            println!("✗ Transfer failed: {}", e);
            println!("  Transaction rolled back");

            let account_a = get_account(&aggregate, account_a_id).await?.unwrap();
            let account_b = get_account(&aggregate, account_b_id).await?.unwrap();

            println!(
                "  Account A: balance={}, version={} (unchanged)",
                account_a.balance, account_a.version
            );
            println!(
                "  Account B: balance={}, version={} (unchanged)",
                account_b.balance, account_b.version
            );
        }
    }
    println!();

    // ========================================================================
    // Scenario 4: Version conflict (optimistic concurrency)
    // ========================================================================
    println!("Demonstrating version conflict...");

    // Get current state
    let account_a = get_account(&aggregate, account_a_id).await?.unwrap();
    let old_version = account_a.version;

    // First transaction: modify the account
    let mut tx1 = aggregate.clone().begin().await?;
    tx1.handle(Command::new(
        account_a_id,
        ApplicationCommand::DebitAccount { amount: 50 },
        None,
        None,
    ))
    .await?;
    tx1.commit().await?;

    // Second transaction: try to modify with old version
    let command_with_old_version = Command::new(
        account_a_id,
        ApplicationCommand::DebitAccount { amount: 100 },
        None,
        Some(old_version), // Stale version!
    );

    let mut tx2 = aggregate.clone().begin().await?;
    let result = tx2.handle(command_with_old_version).await;

    match result {
        Err(e) => {
            println!("✗ Command rejected: {}", e);
            tx2.rollback().await?;
            println!("  Another process modified the account");
        }
        Ok(_) => println!("✗ Version check should have failed!"),
    }
    println!();

    // ========================================================================
    // Scenario 5: Multiple handles on same aggregate (version continuity)
    // ========================================================================
    println!("Demonstrating multiple operations in one transaction...");

    let new_account_id = Uuid::new_v4();
    let mut tx = aggregate.clone().begin().await?;

    // Create account (version 1)
    tx.handle(Command::new(
        new_account_id,
        ApplicationCommand::CreateAccount {
            initial_balance: 1000,
        },
        None,
        None,
    ))
    .await?;
    println!("  Created account with balance=1000");

    // First debit (version 2)
    tx.handle(Command::new(
        new_account_id,
        ApplicationCommand::DebitAccount { amount: 100 },
        None,
        None,
    ))
    .await?;
    println!("  Debited $100 (using cached state)");

    // Second debit (version 3)
    tx.handle(Command::new(
        new_account_id,
        ApplicationCommand::DebitAccount { amount: 50 },
        None,
        None,
    ))
    .await?;
    println!("  Debited $50 (using cached state)");

    // Commit all at once
    tx.commit().await?;

    let new_account = get_account(&aggregate, new_account_id).await?.unwrap();
    println!("✓ Transaction committed");
    println!(
        "  Final balance: {}, version: {}",
        new_account.balance, new_account.version
    );
    println!();

    println!("=== Example Complete ===\n");
    Ok(())
}
